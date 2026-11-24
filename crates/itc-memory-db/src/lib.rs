//! PostgreSQL connection and migration support.

macro_rules! query_sql {
    ($path:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/sql/", $path))
    };
}

mod scope;
mod workflow;

use std::collections::{HashMap, HashSet};

pub use scope::{
    context_pack_under_roots, current_plan_under_roots, decisions_under_roots,
    projects_under_roots, repositories_under_roots, repository_changes_under_roots,
    search_file_lines_under_roots, search_symbols_under_roots, search_under_roots,
};
pub use workflow::{
    CheckpointInput, PlanSyncSummary, TaskReferenceResult, TaskResult, WorkflowError,
    WorkflowPhase, WorkflowPlan, WorkflowState, add_task, add_task_reference,
    current_workflow_state, finish_session, focus_task, record_checkpoint,
    record_checkpoint_under_roots, record_decision, record_decision_under_roots, start_session,
    sync_plan, task_list, update_task_status,
};

use itc_memory_core::DatabaseConfig;
use serde::{Deserialize, Serialize};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use uuid::Uuid;

/// Embedded, forward-only PostgreSQL migrations.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Connects to PostgreSQL using a local itc-memory configuration.
///
/// # Errors
///
/// Returns an error when PostgreSQL cannot be reached or authenticated.
pub async fn connect(config: &DatabaseConfig) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(5)
        .connect_with(connect_options(config))
        .await
}

/// Records inference usage without persisting prompts or model responses.
///
/// # Errors
///
/// Returns an error when the database rejects the usage record.
pub async fn record_model_inference(
    pool: &PgPool,
    input: &ModelInferenceInput,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(query_sql!("lib/record_model_inference_01.sql"))
        .bind(id)
        .bind(&input.task_profile_id)
        .bind(&input.model_id)
        .bind(&input.provider_id)
        .bind(&input.provider_kind)
        .bind(&input.provider_model_id)
        .bind(input.hosted)
        .bind(input.trusted_lan)
        .bind(&input.status)
        .bind(&input.failure_code)
        .bind(input.fallback_used)
        .bind(input.latency_ms)
        .bind(input.input_tokens)
        .bind(input.output_tokens)
        .bind(input.estimated_cost_usd)
        .execute(pool)
        .await?;
    Ok(id)
}

/// Lists aggregate local/hosted inference counts, latency, token usage, and cost estimates.
///
/// # Errors
///
/// Returns an error when the usage aggregate query fails.
pub async fn model_usage_summaries(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<ModelUsageSummary>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/model_usage_summaries_01.sql"))
        .bind(limit.clamp(1, 100))
        .fetch_all(pool)
        .await
}

/// Applies every pending migration to the connected database.
///
/// # Errors
///
/// Returns an error if a migration cannot be recorded or executed.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

/// Returns the latest migration version applied to this database, if any.
///
/// # Errors
///
/// Returns an error when the `SQLx` migration ledger cannot be queried.
pub async fn latest_migration_version(pool: &PgPool) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(query_sql!("lib/latest_migration_version_01.sql"))
        .fetch_one(pool)
        .await
}

/// Verifies that the connected database can execute a query.
///
/// # Errors
///
/// Returns an error if the database rejects the health query.
pub async fn check_connection(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(query_sql!("lib/check_connection_01.sql"))
        .execute(pool)
        .await
        .map(|_| ())
}

/// A project summary suitable for concise CLI output.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct ProjectSummary {
    pub slug: String,
    pub display_name: String,
    pub repositories: i64,
    pub documents: i64,
}
/// A locally indexed repository location.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct RepositorySummary {
    pub display_name: String,
    pub canonical_remote: Option<String>,
    pub root_path: String,
    pub current_branch: Option<String>,
    pub head_sha: Option<String>,
    pub is_dirty: bool,
}

/// One path with an uncommitted local worktree change.
#[derive(Debug, Deserialize, Serialize, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct RepositoryChange {
    pub repository: String,
    pub root_path: String,
    pub path: String,
    pub status_text: String,
    pub is_staged: bool,
    pub is_unstaged: bool,
    pub is_untracked: bool,
}
/// A retrieval result with its provenance.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct SearchResult {
    pub project: String,
    pub root_path: Option<String>,
    pub path: String,
    pub document_kind: String,
    pub title: Option<String>,
    pub excerpt: String,
    pub last_seen_at: String,
    pub freshness_status: String,
    pub reason: String,
    pub score: f32,
}

/// An exact indexed file line or a matching file path.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct FileLineMatch {
    /// Project slug.
    pub project: String,
    /// Exact checkout root.
    pub root_path: String,
    /// Repository-relative file path.
    pub path: String,
    /// Indexed document freshness.
    pub freshness_status: String,
    /// One-based line number, or none for a path-only match.
    pub line_number: Option<i64>,
    /// Exact indexed line, or none for a path-only match.
    pub line: Option<String>,
}

/// A symbol match with its document provenance.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct SymbolResult {
    pub project: String,
    pub root_path: String,
    pub path: String,
    pub name: String,
    pub qualified_name: String,
    pub symbol_kind: String,
    pub line_number: i32,
    pub freshness_status: String,
}

/// A Cargo dependency match and its manifest provenance.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct DependencyResult {
    pub project: String,
    pub repository: String,
    pub root_path: String,
    pub path: String,
    pub dependency_name: String,
    pub requirement: String,
    pub dependency_kind: String,
    pub target: String,
    pub optional: bool,
}

/// A decision record with its source-document provenance.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct DecisionResult {
    pub id: String,
    pub title: String,
    pub rationale: String,
    pub status: String,
    pub decided_at: String,
    pub source_path: Option<String>,
    pub source_root_path: Option<String>,
    pub source_freshness: Option<String>,
    pub is_stale: bool,
}

/// A saved project checkpoint.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct CheckpointResult {
    pub title: String,
    pub summary: String,
    pub next_steps: String,
    pub blockers: String,
    pub created_at: String,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub repository_state: String,
}

/// A recent indexing run with persisted failure counts.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct IndexRunSummary {
    pub root_path: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub discovered_count: i32,
    pub indexed_count: i32,
    pub failed_count: i32,
    pub error_summary: Option<String>,
    pub configuration: serde_json::Value,
}

/// Privacy-preserving metadata for one model provider attempt.
#[derive(Debug)]
pub struct ModelInferenceInput {
    /// Task profile used for the attempt, if one was selected.
    pub task_profile_id: Option<String>,
    /// Stable registered model ID.
    pub model_id: String,
    /// Registered provider ID.
    pub provider_id: String,
    /// Provider protocol identifier.
    pub provider_kind: String,
    /// Provider-specific model identifier.
    pub provider_model_id: String,
    /// Whether this provider is hosted.
    pub hosted: bool,
    /// Whether this provider is explicitly trusted on the private LAN.
    pub trusted_lan: bool,
    /// `succeeded` or `failed`.
    pub status: String,
    /// Safe machine-readable failure category, if any.
    pub failure_code: Option<String>,
    /// Whether the attempt was routed as a fallback.
    pub fallback_used: bool,
    /// Request latency in milliseconds.
    pub latency_ms: i64,
    /// Input token count, when available.
    pub input_tokens: Option<i64>,
    /// Output token count, when available.
    pub output_tokens: Option<i64>,
    /// Estimated cost in USD, only when token counts and rates are available.
    pub estimated_cost_usd: Option<f64>,
}

/// Aggregated inference usage for one task profile/model combination.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct ModelUsageSummary {
    pub task_profile_id: Option<String>,
    pub model_id: String,
    pub provider_id: String,
    pub hosted: bool,
    pub trusted_lan: bool,
    pub call_count: i64,
    pub failure_count: i64,
    pub average_latency_ms: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub estimated_cost_usd: Option<f64>,
    pub last_used_at: String,
}

/// Relevance and size measurements for a generated context pack.
#[derive(Debug, PartialEq, Serialize)]
pub struct ContextMetrics {
    /// Number of selected sources.
    pub source_count: usize,
    /// Total UTF-8 bytes of selected excerpts.
    pub context_bytes: usize,
    /// Mean fused relevance score, or zero when no sources match.
    pub average_score: f32,
    /// Time spent retrieving and assembling the context pack, in microseconds.
    pub retrieval_latency_us: u64,
    /// Maximum excerpt-byte budget supplied to retrieval.
    pub max_context_bytes: usize,
}

/// Retrieval relevance measurements for a labeled query.
#[derive(Debug, PartialEq, Serialize)]
pub struct RetrievalMetrics {
    /// Reciprocal rank of the first relevant source, or zero when none is retrieved.
    pub reciprocal_rank: f32,
    /// Fraction of expected sources found in the first `k` results.
    pub recall_at_k: f32,
    /// Fraction of the first `k` results that are relevant.
    pub precision_at_k: f32,
}

/// Measures ranked retrieval results against a small set of relevant source paths.
#[must_use]
pub fn evaluate_retrieval(
    results: &[SearchResult],
    relevant_paths: &[&str],
    k: usize,
) -> RetrievalMetrics {
    if relevant_paths.is_empty() || k == 0 {
        return RetrievalMetrics {
            reciprocal_rank: 0.0,
            recall_at_k: 0.0,
            precision_at_k: 0.0,
        };
    }

    let relevant = relevant_paths.iter().copied().collect::<HashSet<_>>();
    let first_relevant_rank = results
        .iter()
        .take(k)
        .position(|result| relevant.contains(result.path.as_str()));
    let relevant_found = results
        .iter()
        .take(k)
        .filter(|result| relevant.contains(result.path.as_str()))
        .count();
    let expected_count = usize_as_f32(relevant.len());
    RetrievalMetrics {
        reciprocal_rank: first_relevant_rank.map_or(0.0, |rank| 1.0 / (usize_as_f32(rank) + 1.0)),
        recall_at_k: usize_as_f32(relevant_found) / expected_count,
        precision_at_k: usize_as_f32(relevant_found) / usize_as_f32(k),
    }
}

fn usize_as_f32(value: usize) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

pub(crate) fn fuse_context_candidates(
    channels: Vec<(Vec<SearchResult>, &'static str, f32)>,
    limit: usize,
) -> Vec<SearchResult> {
    const RRF_RANK_OFFSET: f32 = 60.0;
    let mut fused: HashMap<(String, Option<String>, String), SearchResult> = HashMap::new();
    for (results, channel, weight) in channels {
        let mut seen_in_channel = HashSet::new();
        let mut unique_rank = 0_usize;
        for mut result in results {
            let key = (
                result.project.clone(),
                result.root_path.clone(),
                result.path.clone(),
            );
            if !seen_in_channel.insert(key.clone()) {
                continue;
            }
            unique_rank += 1;
            let rank = u16::try_from(unique_rank).unwrap_or(u16::MAX);
            let contribution = weight / (RRF_RANK_OFFSET + f32::from(rank));
            result.reason = format!("{}; channel={channel}", result.reason);
            match fused.entry(key) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let current = entry.get_mut();
                    current.score += contribution;
                    if !current.reason.contains(&format!("channel={channel}")) {
                        current.reason.push_str("; channel=");
                        current.reason.push_str(channel);
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    result.score = contribution;
                    entry.insert(result);
                }
            }
        }
    }

    let mut results = fused.into_values().collect::<Vec<_>>();
    results.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| {
                (right.freshness_status == "current").cmp(&(left.freshness_status == "current"))
            })
            .then_with(|| left.path.cmp(&right.path))
    });
    results.truncate(limit);
    results
}

/// A bounded set of task-relevant sources with explicit provenance.
#[derive(Debug, Serialize)]
pub struct ContextPack {
    /// Project slug used for retrieval.
    pub project: String,
    /// Task text used as the relevance query.
    pub task: String,
    /// Selected source excerpts and their reasons for selection.
    pub sources: Vec<SearchResult>,
    /// Measured relevance and excerpt size.
    pub metrics: ContextMetrics,
}

/// Lists local projects with indexed repository and document counts.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn projects(pool: &PgPool) -> Result<Vec<ProjectSummary>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/projects_01.sql"))
        .fetch_all(pool)
        .await
}
/// Lists repository locations, optionally narrowed to one project slug.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn repositories(
    pool: &PgPool,
    project: Option<&str>,
    dirty: Option<bool>,
) -> Result<Vec<RepositorySummary>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/repositories_01.sql"))
        .bind(project)
        .bind(dirty)
        .fetch_all(pool)
        .await
}

/// Lists current uncommitted paths, optionally narrowed to a project.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn repository_changes(
    pool: &PgPool,
    project: Option<&str>,
) -> Result<Vec<RepositoryChange>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/repository_changes_01.sql"))
        .bind(project)
        .fetch_all(pool)
        .await
}
/// Searches indexed document chunks and paths using PostgreSQL full-text search.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn search(
    pool: &PgPool,
    query: &str,
    project: Option<&str>,
    freshness: Option<&str>,
    limit: i64,
) -> Result<Vec<SearchResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/search_01.sql"))
        .bind(query)
        .bind(project)
        .bind(freshness)
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// Searches extracted Rust symbols by name or qualified path.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn search_symbols(
    pool: &PgPool,
    query: &str,
    project: Option<&str>,
    limit: i64,
) -> Result<Vec<SymbolResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/search_symbols_01.sql"))
        .bind(query)
        .bind(project)
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// Searches indexed Cargo dependency metadata by package name.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn search_dependencies(
    pool: &PgPool,
    query: &str,
    project: Option<&str>,
    limit: i64,
) -> Result<Vec<DependencyResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/search_dependencies_01.sql"))
        .bind(query)
        .bind(project)
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// Searches indexed Cargo dependencies only at allowed checkout locations.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn search_dependencies_under_roots(
    pool: &PgPool,
    query: &str,
    project: Option<&str>,
    limit: i64,
    roots: &[String],
) -> Result<Vec<DependencyResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/search_dependencies_under_roots_01.sql"))
        .bind(query)
        .bind(project)
        .bind(limit)
        .bind(roots)
        .fetch_all(pool)
        .await
}

/// Retrieves the latest current plan document for a project.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn current_plan(pool: &PgPool, project: &str) -> Result<Vec<SearchResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/current_plan_01.sql"))
        .bind(project)
        .fetch_all(pool)
        .await
}

/// Retrieves decisions that have not been superseded.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn decisions(
    pool: &PgPool,
    project: &str,
    limit: i64,
) -> Result<Vec<DecisionResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/decisions_01.sql"))
        .bind(project)
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// Retrieves recent project checkpoints.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn checkpoints(
    pool: &PgPool,
    project: &str,
    limit: i64,
) -> Result<Vec<CheckpointResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/checkpoints_01.sql"))
        .bind(project)
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// Lists recent indexing runs and their persisted failure counts.
///
/// # Errors
/// Returns an error if the query fails.
pub async fn indexing_runs(pool: &PgPool, limit: i64) -> Result<Vec<IndexRunSummary>, sqlx::Error> {
    sqlx::query_as(query_sql!("lib/indexing_runs_01.sql"))
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// Builds a bounded context pack from indexed project documents.
///
/// # Errors
/// Returns an error if retrieval fails.
pub async fn context_pack(
    pool: &PgPool,
    project: &str,
    task: &str,
    max_context_bytes: usize,
) -> Result<ContextPack, sqlx::Error> {
    scope::context_pack_unrestricted(pool, project, task, max_context_bytes).await
}

pub(crate) fn fallback_context_query(task: &str) -> Option<String> {
    const STOP_WORDS: &[&str] = &[
        "a",
        "about",
        "according",
        "an",
        "and",
        "answer",
        "be",
        "based",
        "by",
        "can",
        "context",
        "could",
        "do",
        "for",
        "from",
        "give",
        "how",
        "in",
        "is",
        "it",
        "me",
        "of",
        "on",
        "or",
        "please",
        "provided",
        "say",
        "sentence",
        "source",
        "supplied",
        "tell",
        "that",
        "the",
        "this",
        "to",
        "use",
        "using",
        "what",
        "where",
        "which",
        "with",
        "one",
        "would",
    ];
    let mut terms = Vec::new();
    for token in task.split(|character: char| !character.is_alphanumeric()) {
        let term = token.to_lowercase();
        if (term.chars().count() < 3 && !term.chars().all(char::is_numeric))
            || STOP_WORDS.contains(&term.as_str())
            || terms.contains(&term)
        {
            continue;
        }
        terms.push(term);
        if terms.len() == 12 {
            break;
        }
    }
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

fn bound_context_sources(
    candidates: Vec<SearchResult>,
    max_context_bytes: usize,
) -> (Vec<SearchResult>, ContextMetrics) {
    let mut sources = Vec::new();
    let mut context_bytes = 0;
    for mut source in candidates {
        let remaining = max_context_bytes.saturating_sub(context_bytes);
        if remaining == 0 {
            break;
        }
        if source.excerpt.len() > remaining {
            let mut end = remaining;
            while !source.excerpt.is_char_boundary(end) {
                end -= 1;
            }
            source.excerpt.truncate(end);
        }
        if source.excerpt.is_empty() {
            continue;
        }
        context_bytes += source.excerpt.len();
        sources.push(source);
    }
    let average_score = if sources.is_empty() {
        0.0
    } else {
        let count = u8::try_from(sources.len()).unwrap_or(u8::MAX);
        sources.iter().map(|source| source.score).sum::<f32>() / f32::from(count)
    };
    let metrics = ContextMetrics {
        source_count: sources.len(),
        context_bytes,
        average_score,
        retrieval_latency_us: 0,
        max_context_bytes,
    };
    (sources, metrics)
}

pub(crate) fn connect_options(config: &DatabaseConfig) -> PgConnectOptions {
    PgConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.user)
        .password(&config.password)
        .database(&config.database)
        .application_name("itc-memory")
}

#[cfg(test)]
mod tests;
