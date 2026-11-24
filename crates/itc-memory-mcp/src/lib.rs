//! Shared MCP tools and transports for IDE and local-agent integrations.

use std::{
    collections::BTreeSet,
    env,
    fmt::Display,
    future::Future,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use itc_memory_db::{
    CheckpointInput, CheckpointResult, ContextPack, DecisionResult, DependencyResult,
    ModelInferenceInput, ModelUsageSummary, ProjectSummary, RepositorySummary, SearchResult,
    SymbolResult, checkpoints, context_pack_under_roots, current_plan_under_roots,
    decisions_under_roots, model_usage_summaries, projects_under_roots,
    record_checkpoint_under_roots, record_decision_under_roots, record_model_inference,
    repositories_under_roots, repository_changes_under_roots, search_dependencies_under_roots,
    search_file_lines_under_roots, search_symbols_under_roots, search_under_roots,
};
use itc_memory_index::rust_review::{self, LintResult, ReviewSource};
use itc_memory_model::{
    ExplicitModelRequest, InferenceAttempt, InferenceResult, ModelCard, ModelRegistry,
    ModelTaskProfile, operator_allows_hosted_from_environment,
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerConfig},
    schemars::JsonSchema,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::{
        self,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Validated environment-backed settings for an MCP transport.
pub struct McpSettings {
    /// Roots allowed for every MCP read and write.
    pub roots: Vec<String>,
    /// Whether checkpoint and decision tools may write.
    pub allow_writes: bool,
    /// Whether hosted inference may be considered; each hosted request still needs consent.
    pub allow_hosted: bool,
    /// HTTP bind address, when required by the selected transport.
    pub http_bind: Option<SocketAddr>,
    /// Required bearer token for Streamable HTTP.
    pub http_token: Option<String>,
}

impl McpSettings {
    /// Reads and validates MCP settings from the process environment.
    ///
    /// # Errors
    /// Returns an error when roots or required HTTP settings are invalid.
    pub fn from_environment(http: bool) -> anyhow::Result<Self> {
        Self::from_environment_in(http, None)
    }

    /// Reads settings, resolving relative configuration paths against the environment directory.
    ///
    /// # Errors
    /// Returns an error when roots or required HTTP settings are invalid.
    pub fn from_environment_in(http: bool, config_dir: Option<&Path>) -> anyhow::Result<Self> {
        let parsed_roots = match env::var("ITC_MEMORY_INDEX_ROOTS_FILE") {
            Ok(path) => {
                let path = itc_memory_core::resolve_config_path(path, config_dir);
                let contents = std::fs::read_to_string(&path).map_err(|error| {
                    anyhow::anyhow!(
                        "could not read configured index roots file {}: {error}",
                        path.display()
                    )
                })?;
                parse_allowed_root_file(&contents)?
            }
            Err(env::VarError::NotPresent) => match env::var("ITC_MEMORY_INDEX_ROOTS") {
                Ok(configured_roots) => parse_allowed_roots(&configured_roots)?,
                Err(env::VarError::NotPresent) => {
                    if itc_memory_core::EMBEDDED_INDEX_ROOTS.is_empty() {
                        let root = env::current_dir().map_err(|error| {
                            anyhow::anyhow!(
                                "could not resolve the default repository root: {error}"
                            )
                        })?;
                        parse_allowed_roots(&root.to_string_lossy())?
                    } else {
                        parse_allowed_root_file(itc_memory_core::EMBEDDED_INDEX_ROOTS)?
                    }
                }
                Err(error) => return Err(error.into()),
            },
            Err(error) => return Err(error.into()),
        };
        let roots = parsed_roots
            .into_iter()
            .map(|root| {
                let canonical_root = std::fs::canonicalize(&root).map_err(|error| {
                    anyhow::anyhow!("could not resolve MCP root {root}: {error}")
                })?;
                if !canonical_root.join(".git").exists() {
                    anyhow::bail!(
                        "MCP allowed folder must be an exact Git checkout containing .git: {root}"
                    );
                }
                Ok(canonical_root.to_string_lossy().into_owned())
            })
            .collect::<anyhow::Result<BTreeSet<_>>>()?
            .into_iter()
            .collect();
        let allow_writes = match env::var("ITC_MEMORY_MCP_ALLOW_WRITES")
            .unwrap_or_else(|_| "false".to_owned())
            .as_str()
        {
            "true" => true,
            "false" => false,
            value => {
                anyhow::bail!("ITC_MEMORY_MCP_ALLOW_WRITES must be true or false, got {value}")
            }
        };
        let allow_hosted = operator_allows_hosted_from_environment().map_err(anyhow::Error::msg)?;
        let (http_bind, http_token) = if http {
            let bind = env::var("ITC_MEMORY_MCP_BIND")
                .unwrap_or_else(|_| "127.0.0.1:8765".to_owned())
                .parse()
                .map_err(|error| anyhow::anyhow!("ITC_MEMORY_MCP_BIND is invalid: {error}"))?;
            let token = env::var("ITC_MEMORY_MCP_TOKEN")
                .map_err(|_| anyhow::anyhow!("ITC_MEMORY_MCP_TOKEN is required for HTTP"))?;
            if token.len() < 32 {
                anyhow::bail!("ITC_MEMORY_MCP_TOKEN must contain at least 32 characters");
            }
            (Some(bind), Some(token))
        } else {
            (None, None)
        };
        Ok(Self {
            roots,
            allow_writes,
            allow_hosted,
            http_bind,
            http_token,
        })
    }
}

/// Validates an allowed-root list without touching the filesystem.
///
/// # Errors
/// Returns an error for relative, empty, parent-traversing, or filesystem-root paths.
pub fn parse_allowed_roots(configured: &str) -> anyhow::Result<Vec<String>> {
    let mut roots = BTreeSet::new();
    for raw in configured.split(',') {
        roots.insert(normalize_allowed_root(raw)?);
    }
    if roots.is_empty() {
        anyhow::bail!("ITC_MEMORY_INDEX_ROOTS must contain at least one path");
    }
    Ok(roots.into_iter().collect())
}

fn normalize_allowed_root(raw: &str) -> anyhow::Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("ITC_MEMORY_INDEX_ROOTS contains an empty path");
    }
    let path = Path::new(raw);
    if !path.is_absolute()
        || raw
            .split('/')
            .any(|component| component == "." || component == "..")
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        anyhow::bail!("allowed root must be an absolute, normalized, non-root path: {raw}");
    }
    let normalized_path: PathBuf = path.components().collect();
    if normalized_path == Path::new("/") {
        anyhow::bail!("filesystem root cannot be an MCP allowed root");
    }
    Ok(normalized_path.to_string_lossy().into_owned())
}

fn parse_allowed_root_file(contents: &str) -> anyhow::Result<Vec<String>> {
    let roots = contents
        .lines()
        .map(str::trim)
        .filter(|root| !root.is_empty() && !root.starts_with('#'))
        .map(normalize_allowed_root)
        .collect::<anyhow::Result<BTreeSet<_>>>()?;
    if roots.is_empty() {
        anyhow::bail!("index roots file must list at least one path");
    }
    Ok(roots.into_iter().collect())
}

/// MCP handler that shares the database services used by the CLI.
#[derive(Clone)]
pub struct ToolServer {
    pool: PgPool,
    roots: Vec<String>,
    allow_writes: bool,
    allow_hosted: bool,
    models: ModelRegistry,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl ToolServer {
    /// Creates one handler instance for either transport.
    #[must_use]
    pub fn new(
        pool: PgPool,
        roots: Vec<String>,
        allow_writes: bool,
        allow_hosted: bool,
        models: ModelRegistry,
    ) -> Self {
        Self {
            pool,
            roots,
            allow_writes,
            allow_hosted,
            models,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProjectSearch {
    /// Optional project slug to narrow the results.
    project: Option<String>,
    /// Maximum number of results, capped at 100.
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TextSearch {
    /// Literal text or a file-path fragment.
    query: String,
    /// Optional project slug.
    project: Option<String>,
    /// Optional freshness filter such as `current` or `stale`.
    freshness: Option<String>,
    /// Maximum number of results, capped at 100.
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CodeSearch {
    /// Symbol name, Cargo dependency name, or indexed source terms.
    query: String,
    /// Optional project slug.
    project: Option<String>,
    /// Maximum number of results per result group, capped at 100.
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProjectInput {
    /// Project slug.
    project: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RepositoryInput {
    /// Optional project slug.
    project: Option<String>,
    /// If set, include only dirty or clean repositories.
    dirty: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ContextPackInput {
    /// Project slug.
    project: String,
    /// Task or question used for retrieval.
    task: String,
    /// Maximum excerpt bytes, from 1 through 100000.
    max_context_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecordCheckpointInput {
    /// Project slug.
    project: String,
    /// Short checkpoint title.
    title: String,
    /// Progress summary.
    summary: String,
    /// Suggested next steps.
    next_steps: String,
    /// Current blockers.
    blockers: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecordDecisionInput {
    /// Project slug.
    project: String,
    /// Decision title.
    title: String,
    /// Decision rationale.
    rationale: String,
    /// Optional indexed source path, relative to its repository root.
    source_path: Option<String>,
    /// Optional existing decision identifier to supersede.
    supersedes: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ModelCatalogInput {
    /// Explicitly allow a hosted provider availability check for this request.
    #[serde(default)]
    allow_hosted: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ModelProfileInput {
    /// Configured or currently discovered provider-qualified model ID.
    model_id: String,
    /// Explicitly allow a hosted provider availability check for this request.
    #[serde(default)]
    allow_hosted: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ModelAskInput {
    /// Prompt to send to the selected task profile.
    prompt: String,
    /// Optional project slug whose indexed context should be included.
    project: Option<String>,
    /// Registered task profile; defaults to `ask-explainer`.
    profile: Option<String>,
    /// Optional catalog model ID override. An override disables fallback.
    model_id: Option<String>,
    /// Additional user-supplied context; no filesystem paths are opened.
    context: Option<String>,
    /// Explicitly permit hosted inference for this request.
    #[serde(default)]
    allow_hosted: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RustReviewInput {
    /// Indexed project slug.
    project: String,
    /// Exact allowed checkout root for the project.
    root_path: String,
    /// Review explicit files, current changed files, or the Git diff.
    scope: RustReviewScope,
    /// Repository-relative Rust paths, required only for the files scope.
    files: Option<Vec<String>>,
    /// Explicitly permit hosted inference for this request.
    #[serde(default)]
    allow_hosted: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RustReviewScope {
    Files,
    Changed,
    Diff,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RustLintInput {
    /// Indexed project slug.
    project: String,
    /// Exact allowed checkout root for the project.
    root_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ArchitectureInput {
    /// Project slug used to retrieve local indexed context.
    project: String,
    /// Architecture question or analysis request.
    question: String,
    /// Explicitly permit hosted inference for this request.
    #[serde(default)]
    allow_hosted: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TestGenerationInput {
    /// Source code supplied by the caller.
    code: String,
    /// Optional project slug for indexed context.
    project: Option<String>,
    /// Explicitly permit hosted inference for this request.
    #[serde(default)]
    allow_hosted: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ModelComparisonInput {
    /// First configured or dynamically discovered model ID.
    first_model_id: String,
    /// Second configured or dynamically discovered model ID.
    second_model_id: String,
    /// Prompt sent to both models.
    prompt: String,
    /// Optional project slug whose indexed context should be included.
    project: Option<String>,
    /// Additional user-supplied context.
    context: Option<String>,
    /// Explicitly permit hosted inference for this request.
    #[serde(default)]
    allow_hosted: bool,
}

#[derive(Serialize)]
struct ModelCatalog {
    models: Vec<ModelCard>,
    task_profiles: Vec<ModelTaskProfile>,
    recent_usage: Vec<ModelUsageSummary>,
}

#[derive(Serialize)]
struct ModelProfileOutput {
    card: ModelCard,
    usage: Vec<ModelUsageSummary>,
}

#[derive(Serialize)]
struct ModelRunOutput {
    result: InferenceResult,
    attempts: Vec<InferenceAttempt>,
}

#[derive(Serialize)]
struct RustReviewOutput {
    source: ReviewSource,
    lint: LintResult,
    review: ModelRunOutput,
}

#[derive(Serialize)]
struct ModelComparisonOutput {
    first: ModelComparisonEntry,
    second: ModelComparisonEntry,
}

#[derive(Serialize)]
struct ModelComparisonEntry {
    model_id: String,
    response: Option<String>,
    error: Option<String>,
    attempt: Option<InferenceAttempt>,
}

#[derive(Serialize)]
struct ProjectContext {
    project: ProjectSummary,
    repositories: Vec<RepositorySummary>,
    plan: Vec<SearchResult>,
    decisions: Vec<DecisionResult>,
    checkpoints: Vec<ContextCheckpoint>,
}

#[derive(Serialize)]
struct ContextCheckpoint {
    title: String,
    summary: String,
    next_steps: String,
    blockers: String,
    created_at: String,
    task_id: Option<String>,
    session_id: Option<String>,
}

impl From<CheckpointResult> for ContextCheckpoint {
    fn from(checkpoint: CheckpointResult) -> Self {
        Self {
            title: checkpoint.title,
            summary: checkpoint.summary,
            next_steps: checkpoint.next_steps,
            blockers: checkpoint.blockers,
            created_at: checkpoint.created_at,
            task_id: checkpoint.task_id,
            session_id: checkpoint.session_id,
        }
    }
}

#[derive(Serialize)]
struct CodeSearchResults {
    symbols: Vec<SymbolResult>,
    dependencies: Vec<DependencyResult>,
    indexed_matches: Vec<SearchResult>,
}

#[tool_router]
impl ToolServer {
    /// Lists indexed projects available through configured checkouts.
    #[tool(
        name = "search_projects",
        description = "List indexed projects available through the configured checkout allowlist."
    )]
    async fn search_projects(
        &self,
        Parameters(input): Parameters<ProjectSearch>,
    ) -> Result<String, ErrorData> {
        let projects = projects_under_roots(&self.pool, &self.roots)
            .await
            .map_err(internal_error)?;
        let projects: Vec<_> = projects
            .into_iter()
            .filter(|item| input.project.as_ref().is_none_or(|slug| item.slug == *slug))
            .take(result_limit(input.limit))
            .collect();
        json_output(&projects)
    }

    /// Finds exact indexed lines and paths under configured roots.
    #[tool(
        name = "search_files",
        description = "Search indexed file paths and exact, case-sensitive lines from configured checkouts. Path-only matches have no line number."
    )]
    async fn search_files(
        &self,
        Parameters(input): Parameters<TextSearch>,
    ) -> Result<String, ErrorData> {
        require_nonempty(&input.query, "query")?;
        if input.query.contains(['\n', '\r']) {
            return Err(ErrorData::invalid_params(
                "search_files requires a single-line literal query",
                None,
            ));
        }
        let results = search_file_lines_under_roots(
            &self.pool,
            &input.query,
            input.project.as_deref(),
            input.freshness.as_deref(),
            result_limit_i64(input.limit),
            &self.roots,
        )
        .await
        .map_err(internal_error)?;
        if results
            .iter()
            .filter_map(|result| result.line.as_ref())
            .map(String::len)
            .sum::<usize>()
            > 100_000
        {
            return Err(internal_error(
                "exact matches exceed the response size limit; narrow the query or limit",
            ));
        }
        json_output(&results)
    }

    /// Searches symbols, Cargo dependencies, and indexed code text from allowed checkouts.
    #[tool(
        name = "search_code",
        description = "Search indexed Rust symbols, Cargo dependencies, and source text from configured checkouts."
    )]
    async fn search_code(
        &self,
        Parameters(input): Parameters<CodeSearch>,
    ) -> Result<String, ErrorData> {
        require_nonempty(&input.query, "query")?;
        let limit = result_limit_i64(input.limit);
        let (symbols, dependencies, indexed_matches) = tokio::try_join!(
            search_symbols_under_roots(
                &self.pool,
                &input.query,
                input.project.as_deref(),
                limit,
                &self.roots
            ),
            search_dependencies_under_roots(
                &self.pool,
                &input.query,
                input.project.as_deref(),
                limit,
                &self.roots
            ),
            search_under_roots(
                &self.pool,
                &input.query,
                input.project.as_deref(),
                Some("current"),
                limit,
                &self.roots
            )
        )
        .map_err(internal_error)?;
        json_output(&CodeSearchResults {
            symbols,
            dependencies,
            indexed_matches,
        })
    }

    /// Returns a compact project overview, excluding historical absolute-path snapshots.
    #[tool(
        name = "get_project_context",
        description = "Retrieve project repositories, current plan, decisions, and checkpoint summaries scoped to configured checkouts."
    )]
    async fn get_project_context(
        &self,
        Parameters(input): Parameters<ProjectInput>,
    ) -> Result<String, ErrorData> {
        let project = self.ensure_project(&input.project).await?;
        let (repositories, plan, decisions, checkpoints) = tokio::try_join!(
            repositories_under_roots(&self.pool, Some(&input.project), None, &self.roots),
            current_plan_under_roots(&self.pool, &input.project, &self.roots),
            decisions_under_roots(&self.pool, &input.project, 20, &self.roots),
            checkpoints(&self.pool, &input.project, 5)
        )
        .map_err(internal_error)?;
        json_output(&ProjectContext {
            project,
            repositories,
            plan,
            decisions,
            checkpoints: checkpoints.into_iter().map(Into::into).collect(),
        })
    }

    /// Retrieves the current indexed plan from configured checkouts.
    #[tool(
        name = "get_current_plan",
        description = "Retrieve the current indexed plan for a project, restricted to configured roots."
    )]
    async fn get_current_plan(
        &self,
        Parameters(input): Parameters<ProjectInput>,
    ) -> Result<String, ErrorData> {
        self.ensure_project(&input.project).await?;
        let plan = current_plan_under_roots(&self.pool, &input.project, &self.roots)
            .await
            .map_err(internal_error)?;
        json_output(&plan)
    }

    /// Retrieves decisions for an allowed project, including source freshness.
    #[tool(
        name = "get_project_decisions",
        description = "Retrieve current project decisions and source freshness, scoped to configured roots."
    )]
    async fn get_project_decisions(
        &self,
        Parameters(input): Parameters<ProjectInput>,
    ) -> Result<String, ErrorData> {
        self.ensure_project(&input.project).await?;
        let decisions = decisions_under_roots(&self.pool, &input.project, 50, &self.roots)
            .await
            .map_err(internal_error)?;
        json_output(&decisions)
    }

    /// Lists repository status only for configured checkouts.
    #[tool(
        name = "get_repository_status",
        description = "List indexed repository status for configured checkouts, optionally filtered by project or dirty state."
    )]
    async fn get_repository_status(
        &self,
        Parameters(input): Parameters<RepositoryInput>,
    ) -> Result<String, ErrorData> {
        if let Some(project) = input.project.as_deref() {
            self.ensure_project(project).await?;
        }
        let repositories = repositories_under_roots(
            &self.pool,
            input.project.as_deref(),
            input.dirty,
            &self.roots,
        )
        .await
        .map_err(internal_error)?;
        json_output(&repositories)
    }

    /// Lists locally indexed changes only for configured checkouts.
    #[tool(
        name = "get_local_changes",
        description = "List indexed local changes for configured checkouts."
    )]
    async fn get_local_changes(
        &self,
        Parameters(input): Parameters<ProjectInput>,
    ) -> Result<String, ErrorData> {
        self.ensure_project(&input.project).await?;
        let changes = repository_changes_under_roots(&self.pool, Some(&input.project), &self.roots)
            .await
            .map_err(internal_error)?;
        json_output(&changes)
    }

    /// Builds a bounded context pack with source references and relevance metrics.
    #[tool(
        name = "build_context_pack",
        description = "Build a relevance-ranked, byte-bounded context pack from current indexed sources in configured checkouts."
    )]
    async fn build_context_pack(
        &self,
        Parameters(input): Parameters<ContextPackInput>,
    ) -> Result<String, ErrorData> {
        self.ensure_project(&input.project).await?;
        require_nonempty(&input.task, "task")?;
        let max_context_bytes = input.max_context_bytes.unwrap_or(16_000);
        if !(1..=100_000).contains(&max_context_bytes) {
            return Err(ErrorData::invalid_params(
                "max_context_bytes must be from 1 through 100000",
                None,
            ));
        }
        let pack: ContextPack = context_pack_under_roots(
            &self.pool,
            &input.project,
            &input.task,
            max_context_bytes,
            &self.roots,
        )
        .await
        .map_err(internal_error)?;
        json_output(&pack)
    }

    /// Records a checkpoint only when mutation permission is explicitly enabled.
    #[tool(
        name = "record_checkpoint",
        description = "Record a project checkpoint with a repository snapshot restricted to configured roots; requires ITC_MEMORY_MCP_ALLOW_WRITES=true."
    )]
    async fn record_checkpoint(
        &self,
        Parameters(input): Parameters<RecordCheckpointInput>,
    ) -> Result<String, ErrorData> {
        self.require_writes()?;
        self.ensure_project(&input.project).await?;
        let id = record_checkpoint_under_roots(
            &self.pool,
            CheckpointInput {
                project: &input.project,
                title: &input.title,
                summary: &input.summary,
                next_steps: &input.next_steps,
                blockers: &input.blockers,
                task_id: None,
                session_id: None,
            },
            &self.roots,
        )
        .await
        .map_err(internal_error)?;
        json_output(&serde_json::json!({ "checkpoint_id": id }))
    }

    /// Records a decision only when mutation permission is explicitly enabled.
    #[tool(
        name = "record_decision",
        description = "Record a project decision; optional source documents must belong to configured checkouts. Requires ITC_MEMORY_MCP_ALLOW_WRITES=true."
    )]
    async fn record_decision(
        &self,
        Parameters(input): Parameters<RecordDecisionInput>,
    ) -> Result<String, ErrorData> {
        self.require_writes()?;
        self.ensure_project(&input.project).await?;
        let supersedes = input
            .supersedes
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        let id = record_decision_under_roots(
            &self.pool,
            &input.project,
            &input.title,
            &input.rationale,
            input.source_path.as_deref(),
            supersedes,
            &self.roots,
        )
        .await
        .map_err(internal_error)?;
        json_output(&serde_json::json!({ "decision_id": id }))
    }

    /// Lists configured and dynamically discovered model cards, task profiles, and usage.
    #[tool(
        name = "list_local_models",
        description = "List configured and dynamically discovered local or trusted-LAN model cards and task-profile assignments. Hosted availability checks require operator and per-request consent."
    )]
    async fn list_local_models(
        &self,
        Parameters(input): Parameters<ModelCatalogInput>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        let models = cancellable(
            &request_context.ct,
            self.models
                .model_cards(input.allow_hosted, self.allow_hosted),
        )
        .await?;
        let recent_usage = model_usage_summaries(&self.pool, 100)
            .await
            .map_err(internal_error)?;
        json_output(&ModelCatalog {
            models,
            task_profiles: self.models.task_profiles().to_vec(),
            recent_usage,
        })
    }

    /// Retrieves one configured or dynamically discovered model card and its usage.
    #[tool(
        name = "get_model_profile",
        description = "Retrieve a configured or dynamically discovered model card, provider scope, availability, assigned task roles, and usage metrics."
    )]
    async fn get_model_profile(
        &self,
        Parameters(input): Parameters<ModelProfileInput>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        require_nonempty(&input.model_id, "model_id")?;
        let card = cancellable(
            &request_context.ct,
            self.models
                .model_cards(input.allow_hosted, self.allow_hosted),
        )
        .await?
        .into_iter()
        .find(|card| card.model.id == input.model_id)
        .ok_or_else(|| {
            ErrorData::invalid_params(
                "model is not present in the current provider catalogue",
                None,
            )
        })?;
        let usage = model_usage_summaries(&self.pool, 100)
            .await
            .map_err(internal_error)?
            .into_iter()
            .filter(|item| item.model_id == input.model_id)
            .collect::<Vec<_>>();
        json_output(&ModelProfileOutput { card, usage })
    }

    /// Answers a question using a selected task profile and optional catalog model.
    #[tool(
        name = "ask_local_model",
        description = "Route a prompt through a registered task profile, optionally adding indexed project context. Hosted inference is disabled unless both privacy gates are set."
    )]
    async fn ask_local_model(
        &self,
        Parameters(input): Parameters<ModelAskInput>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        require_nonempty(&input.prompt, "prompt")?;
        let profile = input.profile.as_deref().unwrap_or("ask-explainer");
        let context = self
            .model_context(
                input.project.as_deref(),
                &input.prompt,
                input.context.as_deref(),
            )
            .await?;
        self.run_profile(
            profile,
            input.model_id.as_deref(),
            &input.prompt,
            context.as_deref(),
            input.allow_hosted,
            &request_context.ct,
        )
        .await
    }

    /// Runs deterministic Clippy checks for an allowed project checkout.
    #[tool(
        name = "lint_rust",
        description = "Run cargo clippy for an indexed project checkout and return compiler diagnostics. This executes the checkout's build scripts and may write build artifacts."
    )]
    async fn lint_rust(
        &self,
        Parameters(input): Parameters<RustLintInput>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        let root = self.review_root(&input.project, &input.root_path).await?;
        let lint = rust_review::lint(&root, &request_context.ct)
            .await
            .map_err(internal_error)?;
        json_output(&lint)
    }

    /// Reviews selected checkout source and Clippy diagnostics with the Rust-review profile.
    #[tool(
        name = "review_rust",
        description = "Review selected Rust files or current changes in an indexed checkout. Retrieves source, runs cargo clippy, and delegates source and diagnostics to the Rust-review profile. This executes build scripts and may write build artifacts."
    )]
    async fn review_rust(
        &self,
        Parameters(input): Parameters<RustReviewInput>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        let root = self.review_root(&input.project, &input.root_path).await?;
        let source = match input.scope {
            RustReviewScope::Files => rust_review::explicit_files(
                &root,
                input.files.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("files scope requires a files list", None)
                })?,
            )
            .map_err(internal_error)?,
            RustReviewScope::Changed | RustReviewScope::Diff => {
                if input.files.is_some() {
                    return Err(ErrorData::invalid_params(
                        "files are only valid with the files scope",
                        None,
                    ));
                }
                rust_review::changed_files(&root, &request_context.ct)
                    .await
                    .map_err(internal_error)?
            }
        };
        let lint = rust_review::lint(&root, &request_context.ct)
            .await
            .map_err(internal_error)?;
        if !lint.success && lint.diagnostics.is_empty() {
            return Err(internal_error(format!(
                "Clippy failed with exit code {:?} before producing diagnostics: {}",
                lint.exit_code, lint.stderr
            )));
        }
        let diagnostics = serde_json::to_string(&lint).map_err(internal_error)?;
        if diagnostics.len() > 20_000 {
            return Err(internal_error(
                "Clippy diagnostics exceed the review input limit; call lint_rust to inspect them",
            ));
        }
        let prompt = format!(
            "Review the supplied Rust source and diff for actionable correctness defects. \
             Cite file paths and line numbers, and reference relevant Clippy diagnostics. \
             Distinguish lint diagnostics from your own findings. Clippy may have failed; \
             do not claim a successful lint unless its success field is true.\n\
             Source: {}\nClippy: {diagnostics}",
            serde_json::to_string(&source).map_err(internal_error)?
        );
        let context = self
            .model_context(
                Some(&input.project),
                "Rust review and relevant code conventions",
                None,
            )
            .await?;
        let review = self
            .execute_profile(
                "rust-reviewer",
                None,
                &prompt,
                context.as_deref(),
                input.allow_hosted,
                &request_context.ct,
            )
            .await?;
        json_output(&RustReviewOutput {
            source,
            lint,
            review,
        })
    }

    /// Analyzes architecture using current indexed context for an allowed project.
    #[tool(
        name = "analyze_architecture",
        description = "Analyze a project's architecture using only indexed context from configured checkouts."
    )]
    async fn analyze_architecture(
        &self,
        Parameters(input): Parameters<ArchitectureInput>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        require_nonempty(&input.question, "question")?;
        let context = self
            .model_context(Some(&input.project), &input.question, None)
            .await?;
        self.run_profile(
            "architecture-analyst",
            None,
            &input.question,
            context.as_deref(),
            input.allow_hosted,
            &request_context.ct,
        )
        .await
    }

    /// Suggests focused tests for caller-supplied source code.
    #[tool(
        name = "generate_tests",
        description = "Suggest candidate tests for caller-supplied code using the registered test-designer profile."
    )]
    async fn generate_tests(
        &self,
        Parameters(input): Parameters<TestGenerationInput>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        require_nonempty(&input.code, "code")?;
        let prompt = format!(
            "Suggest focused tests for this code. Do not claim they were run:\n```\n{}\n```",
            input.code
        );
        let context = self
            .model_context(
                input.project.as_deref(),
                "test conventions and coverage for this source",
                None,
            )
            .await?;
        self.run_profile(
            "test-designer",
            None,
            &prompt,
            context.as_deref(),
            input.allow_hosted,
            &request_context.ct,
        )
        .await
    }

    /// Runs two configured or dynamically discovered models against the same prompt.
    #[tool(
        name = "compare_models",
        description = "Compare two configured or dynamically discovered model responses and usage. Any hosted model requires operator and per-request consent."
    )]
    async fn compare_models(
        &self,
        Parameters(input): Parameters<ModelComparisonInput>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        require_nonempty(&input.prompt, "prompt")?;
        if input.first_model_id == input.second_model_id {
            return Err(ErrorData::invalid_params(
                "first_model_id and second_model_id must be different",
                None,
            ));
        }
        let context = self
            .model_context(
                input.project.as_deref(),
                &input.prompt,
                input.context.as_deref(),
            )
            .await?;
        let (first, second) = cancellable(&request_context.ct, async {
            tokio::join!(
                self.models.run_registered_model(ExplicitModelRequest {
                    model_id: &input.first_model_id,
                    task_profile_id: None,
                    prompt: &input.prompt,
                    context: context.as_deref(),
                    allow_hosted: input.allow_hosted,
                    operator_allows_hosted: self.allow_hosted,
                    fallback_used: false,
                }),
                self.models.run_registered_model(ExplicitModelRequest {
                    model_id: &input.second_model_id,
                    task_profile_id: None,
                    prompt: &input.prompt,
                    context: context.as_deref(),
                    allow_hosted: input.allow_hosted,
                    operator_allows_hosted: self.allow_hosted,
                    fallback_used: false,
                })
            )
        })
        .await?;
        self.record_model_attempt(first.attempt.as_ref()).await?;
        self.record_model_attempt(second.attempt.as_ref()).await?;
        json_output(&ModelComparisonOutput {
            first: comparison_entry(input.first_model_id, first),
            second: comparison_entry(input.second_model_id, second),
        })
    }

    async fn model_context(
        &self,
        project: Option<&str>,
        task: &str,
        extra_context: Option<&str>,
    ) -> Result<Option<String>, ErrorData> {
        let mut sections = Vec::new();
        if let Some(context) = extra_context.filter(|value| !value.trim().is_empty()) {
            sections.push(context.to_owned());
        }
        if let Some(project) = project {
            self.ensure_project(project).await?;
            let pack = context_pack_under_roots(&self.pool, project, task, 16_000, &self.roots)
                .await
                .map_err(internal_error)?;
            sections.push(serde_json::to_string(&pack).map_err(internal_error)?);
        }
        if sections.is_empty() {
            Ok(None)
        } else {
            Ok(Some(sections.join("\n\n")))
        }
    }

    async fn run_profile(
        &self,
        profile_id: &str,
        model_id: Option<&str>,
        prompt: &str,
        context: Option<&str>,
        allow_hosted: bool,
        cancellation: &CancellationToken,
    ) -> Result<String, ErrorData> {
        let result = self
            .execute_profile(
                profile_id,
                model_id,
                prompt,
                context,
                allow_hosted,
                cancellation,
            )
            .await?;
        json_output(&result)
    }

    async fn execute_profile(
        &self,
        profile_id: &str,
        model_id: Option<&str>,
        prompt: &str,
        context: Option<&str>,
        allow_hosted: bool,
        cancellation: &CancellationToken,
    ) -> Result<ModelRunOutput, ErrorData> {
        let routed = cancellable(
            cancellation,
            self.models.run_profile(
                profile_id,
                model_id,
                prompt,
                context,
                allow_hosted,
                self.allow_hosted,
            ),
        )
        .await?;
        self.record_model_attempts(&routed.attempts).await?;
        match routed.result {
            Ok(result) => Ok(ModelRunOutput {
                result,
                attempts: routed.attempts,
            }),
            Err(error) => Err(internal_error(error)),
        }
    }

    async fn record_model_attempts(&self, attempts: &[InferenceAttempt]) -> Result<(), ErrorData> {
        for attempt in attempts {
            self.record_model_attempt(Some(attempt)).await?;
        }
        Ok(())
    }

    async fn record_model_attempt(
        &self,
        attempt: Option<&InferenceAttempt>,
    ) -> Result<(), ErrorData> {
        let Some(attempt) = attempt else {
            return Ok(());
        };
        let latency_ms = i64::try_from(attempt.latency_ms)
            .map_err(|error| internal_error(format!("model latency is out of range: {error}")))?;
        let input_tokens = attempt
            .input_tokens
            .map(i64::try_from)
            .transpose()
            .map_err(|error| {
                internal_error(format!("input token count is out of range: {error}"))
            })?;
        let output_tokens = attempt
            .output_tokens
            .map(i64::try_from)
            .transpose()
            .map_err(|error| {
                internal_error(format!("output token count is out of range: {error}"))
            })?;
        record_model_inference(
            &self.pool,
            &ModelInferenceInput {
                task_profile_id: attempt.task_profile_id.clone(),
                model_id: attempt.model_id.clone(),
                provider_id: attempt.provider_id.clone(),
                provider_kind: attempt.provider_kind.as_str().to_owned(),
                provider_model_id: attempt.provider_model_id.clone(),
                hosted: attempt.hosted,
                trusted_lan: attempt.trusted_lan,
                status: attempt.status.clone(),
                failure_code: attempt.failure_code.clone(),
                fallback_used: attempt.fallback_used,
                latency_ms,
                input_tokens,
                output_tokens,
                estimated_cost_usd: attempt.estimated_cost_usd,
            },
        )
        .await
        .map_err(internal_error)?;
        Ok(())
    }
}

#[tool_handler]
#[allow(clippy::unused_async_trait_impl)]
impl ServerHandler for ToolServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Indexed reads and review source selection are limited to configured checkouts. Database mutations require operator permission. lint_rust and review_rust run unsandboxed Cargo build code and may write artifacts; use only with trusted checkouts.")
    }
}

impl ToolServer {
    async fn review_root(&self, project: &str, root_path: &str) -> Result<PathBuf, ErrorData> {
        require_nonempty(project, "project")?;
        require_nonempty(root_path, "root_path")?;
        if !self.roots.iter().any(|root| root == root_path) {
            return Err(ErrorData::invalid_params(
                "root_path must be an exact configured checkout root",
                None,
            ));
        }
        let repositories = repositories_under_roots(&self.pool, Some(project), None, &self.roots)
            .await
            .map_err(internal_error)?;
        if !repositories
            .iter()
            .any(|repository| repository.root_path == root_path)
        {
            return Err(ErrorData::invalid_params(
                "root_path is not an indexed checkout of the requested project",
                None,
            ));
        }
        let root = PathBuf::from(root_path);
        let canonical = std::fs::canonicalize(&root).map_err(internal_error)?;
        if canonical != root || !root.join(".git").exists() {
            return Err(ErrorData::invalid_params(
                "checkout root has changed or is no longer a Git checkout",
                None,
            ));
        }
        Ok(root)
    }

    async fn ensure_project(&self, slug: &str) -> Result<ProjectSummary, ErrorData> {
        let projects = projects_under_roots(&self.pool, &self.roots)
            .await
            .map_err(internal_error)?;
        projects
            .into_iter()
            .find(|project| project.slug == slug)
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    "project is not available through configured checkouts",
                    None,
                )
            })
    }

    fn require_writes(&self) -> Result<(), ErrorData> {
        if self.allow_writes {
            Ok(())
        } else {
            Err(ErrorData::invalid_params(
                "mutating tools are disabled; set ITC_MEMORY_MCP_ALLOW_WRITES=true to enable",
                None,
            ))
        }
    }
}

/// Serves the shared tool handler over MCP stdio.
///
/// # Errors
/// Returns an error when the MCP transport fails.
pub async fn run_stdio(server: ToolServer) -> anyhow::Result<()> {
    server.serve(transport::stdio()).await?.waiting().await?;
    Ok(())
}

/// Serves the shared tool handler over authenticated Streamable HTTP.
///
/// # Errors
/// Returns an error when binding or serving the HTTP listener fails.
pub async fn run_http(server: ToolServer, bind: SocketAddr, token: String) -> anyhow::Result<()> {
    validate_http_bind(bind)?;
    if token.len() < 32 {
        anyhow::bail!("HTTP bearer token must contain at least 32 characters");
    }
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let auth = HttpAuth {
        token: Arc::from(token),
        bound_address: bind,
    };
    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(auth, authorize_http));
    let listener = TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn validate_http_bind(bind: SocketAddr) -> anyhow::Result<()> {
    if !bind.ip().is_loopback() {
        anyhow::bail!(
            "the MCP HTTP listener must bind to loopback; use a TLS-terminating reverse proxy for remote clients"
        );
    }
    Ok(())
}

#[derive(Clone)]
struct HttpAuth {
    token: Arc<str>,
    bound_address: SocketAddr,
}

async fn authorize_http(
    State(auth): State<HttpAuth>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let headers = request.headers();
    if !valid_bearer_token(headers, &auth.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !safe_http_origin(headers, auth.bound_address) {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

fn valid_bearer_token(headers: &HeaderMap, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    expected.as_bytes().ct_eq(provided.as_bytes()).into()
}

fn safe_http_origin(headers: &HeaderMap, bound_address: SocketAddr) -> bool {
    if headers.contains_key(header::ORIGIN) {
        return false;
    }
    if !bound_address.ip().is_loopback() {
        return false;
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let acceptable_hosts = [
        format!("127.0.0.1:{}", bound_address.port()),
        format!("localhost:{}", bound_address.port()),
        format!("[::1]:{}", bound_address.port()),
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "[::1]".to_owned(),
    ];
    acceptable_hosts
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
}

fn result_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(20).clamp(1, 100) as usize
}

fn result_limit_i64(limit: Option<u32>) -> i64 {
    i64::try_from(result_limit(limit)).unwrap_or(100)
}

fn require_nonempty(value: &str, name: &str) -> Result<(), ErrorData> {
    if value.trim().is_empty() {
        Err(ErrorData::invalid_params(
            format!("{name} cannot be empty"),
            None,
        ))
    } else {
        Ok(())
    }
}

async fn cancellable<T>(
    cancellation: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, ErrorData> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ErrorData::internal_error("request was cancelled", None)),
        result = future => Ok(result),
    }
}

fn internal_error(error: impl Display) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

fn json_output(value: &impl Serialize) -> Result<String, ErrorData> {
    serde_json::to_string(value).map_err(internal_error)
}

fn comparison_entry(model_id: String, call: itc_memory_model::ModelCall) -> ModelComparisonEntry {
    let attempt = call.attempt;
    match call.result {
        Ok(result) => ModelComparisonEntry {
            model_id,
            response: Some(result.response),
            error: None,
            attempt,
        },
        Err(error) => ModelComparisonEntry {
            model_id,
            response: None,
            error: Some(error.to_string()),
            attempt,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_cancellation_drops_the_active_provider_future() {
        struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let cancellation = CancellationToken::new();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (dropped_sender, dropped_receiver) = tokio::sync::oneshot::channel();
        let provider_future = async move {
            let _signal = DropSignal(Some(dropped_sender));
            let _ = started_sender.send(());
            std::future::pending::<()>().await;
        };
        let task_cancellation = cancellation.clone();
        let task =
            tokio::spawn(async move { cancellable(&task_cancellation, provider_future).await });

        started_receiver.await.unwrap();
        cancellation.cancel();

        assert!(task.await.unwrap().is_err());
        dropped_receiver.await.unwrap();
    }

    #[test]
    fn allowed_roots_are_normalized_deduplicated_and_reject_parent_paths() {
        assert_eq!(
            parse_allowed_roots("/work/repo/, /work//repo").unwrap(),
            vec!["/work/repo"]
        );
        assert!(parse_allowed_roots("relative/path").is_err());
        assert!(parse_allowed_roots("/work/../private").is_err());
        assert!(parse_allowed_roots("/work/./repo").is_err());
        assert!(parse_allowed_roots("/").is_err());
        assert!(parse_allowed_roots("/work,,/other").is_err());
        assert_eq!(
            parse_allowed_root_file("# selected roots\n/work/repo\n\n/other/repo\n").unwrap(),
            vec!["/other/repo", "/work/repo"]
        );
        assert_eq!(
            parse_allowed_root_file("/work/repo,with-comma\n").unwrap(),
            vec!["/work/repo,with-comma"]
        );
        assert!(parse_allowed_root_file("# no selected roots\n").is_err());
    }

    #[test]
    fn loopback_http_rejects_browser_origins_and_untrusted_hosts() {
        let address: SocketAddr = "127.0.0.1:8765".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:8765".parse().unwrap());
        assert!(safe_http_origin(&headers, address));
        headers.insert(header::ORIGIN, "http://localhost:8765".parse().unwrap());
        assert!(!safe_http_origin(&headers, address));
        headers.remove(header::ORIGIN);
        headers.insert(
            header::HOST,
            "localhost.attacker.test:8765".parse().unwrap(),
        );
        assert!(!safe_http_origin(&headers, address));
    }

    #[test]
    fn http_listener_rejects_non_loopback_bind_addresses() {
        assert!(validate_http_bind("127.0.0.1:8765".parse().unwrap()).is_ok());
        assert!(validate_http_bind("0.0.0.0:8765".parse().unwrap()).is_err());
    }

    #[test]
    fn http_requires_the_exact_configured_bearer_token() {
        let mut headers = HeaderMap::new();
        assert!(!valid_bearer_token(&headers, "configured-secret"));
        headers.insert(
            header::AUTHORIZATION,
            "Bearer wrong-secret".parse().unwrap(),
        );
        assert!(!valid_bearer_token(&headers, "configured-secret"));
        headers.insert(
            header::AUTHORIZATION,
            "Bearer configured-secret".parse().unwrap(),
        );
        assert!(valid_bearer_token(&headers, "configured-secret"));
    }

    #[tokio::test]
    async fn mutating_tools_are_denied_by_default() {
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(
            sqlx::postgres::PgConnectOptions::new()
                .host("127.0.0.1")
                .database("itc_memory")
                .username("test"),
        );
        let config = serde_json::from_str(include_str!("../tests/fixtures/models.json")).unwrap();
        let model_registry = ModelRegistry::new(config).unwrap();
        let server = ToolServer::new(
            pool,
            vec!["/tmp/itc-memory-test".to_owned()],
            false,
            false,
            model_registry,
        );
        assert!(server.require_writes().is_err());
    }

    #[test]
    fn tool_contract_contains_initial_phase_six_and_seven_tools() {
        let tools = ToolServer::tool_router().list_all();
        let names: BTreeSet<_> = tools
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(
            names,
            [
                "build_context_pack",
                "get_current_plan",
                "get_local_changes",
                "get_project_context",
                "get_project_decisions",
                "get_repository_status",
                "get_model_profile",
                "record_checkpoint",
                "record_decision",
                "list_local_models",
                "lint_rust",
                "search_code",
                "search_files",
                "search_projects",
                "ask_local_model",
                "review_rust",
                "analyze_architecture",
                "generate_tests",
                "compare_models",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );
    }
}
