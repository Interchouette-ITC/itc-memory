//! Command-line entry point for itc-memory.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use itc_memory_core::DatabaseConfig;
use itc_memory_index::rust_review::{self, LintResult, ReviewScope, ReviewSource};
use itc_memory_model::{
    Availability, ExplicitModelRequest, InferenceAttempt, ModelCall, ModelCard, ModelRegistry,
    ModelTaskProfile, RoutedInference, operator_allows_hosted_from_environment,
};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// Local-first project intelligence.
#[derive(Debug, Parser)]
#[command(
    name = "itc-memory",
    version,
    about,
    after_help = "Use `itc-memory <command> --help` for details. Find project slugs with `itc-memory project list --json`. Use --json for structured output where available."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// CLI operations.
#[derive(Debug, Subcommand)]
enum Command {
    /// Operate on the canonical PostgreSQL schema.
    Database {
        #[command(subcommand)]
        command: DatabaseCommand,
    },
    /// Discover local Git repositories and refresh changed files.
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
    /// Inspect indexed projects and repository locations.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Inspect local repository locations and worktree changes.
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Retrieve the current indexed project plan.
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Create and update independent project tasks.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Retrieve a combined agent recovery state.
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommand,
    },
    /// Record a project checkpoint or handoff.
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommand,
    },
    /// Record a validated project decision.
    Decision {
        #[command(subcommand)]
        command: DecisionCommand,
    },
    /// Track an agent session independently of plans and tasks.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Retrieve current project decisions.
    Decisions {
        project: String,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 12)]
        limit: i64,
    },
    /// Retrieve recent project checkpoints.
    Checkpoints {
        project: String,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 12)]
        limit: i64,
    },
    /// Search extracted Rust symbols.
    Symbols {
        query: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Search indexed Cargo dependencies.
    Dependencies {
        query: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Search indexed documentation and plans.
    Search {
        query: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, value_enum)]
        freshness: Option<FreshnessArg>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 12)]
        limit: i64,
    },
    /// Find exact indexed lines or matching file paths.
    SearchFiles {
        query: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, value_enum)]
        freshness: Option<FreshnessArg>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Search Rust symbols, Cargo dependencies, and indexed text together.
    SearchCode {
        /// Symbol name, dependency name, or indexed text query.
        query: String,
        /// Restrict results to this project slug.
        #[arg(long)]
        project: Option<String>,
        /// Maximum results per group, from 1 through 100.
        #[arg(long, default_value_t = 20)]
        limit: i64,
        /// Return structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run Clippy on an indexed Rust checkout.
    LintRust {
        /// Project slug from `project list`.
        project: String,
        /// Absolute path of an indexed checkout belonging to the project.
        #[arg(long)]
        root_path: PathBuf,
        /// Return diagnostics as JSON, including when Clippy fails.
        #[arg(long)]
        json: bool,
    },
    /// Review selected Rust files or current changes using the configured review model.
    ReviewRust {
        /// Project slug from `project list`.
        project: String,
        /// Absolute path of an indexed checkout belonging to the project.
        #[arg(long)]
        root_path: PathBuf,
        /// Repository-relative Rust file; repeat to review multiple files.
        #[arg(
            long = "file",
            required_unless_present = "changed",
            conflicts_with = "changed"
        )]
        files: Vec<String>,
        /// Review staged, unstaged, and untracked Rust changes.
        #[arg(long, conflicts_with = "files")]
        changed: bool,
        /// Permit hosted inference when the operator also allows it.
        #[arg(long)]
        allow_hosted: bool,
        /// Return structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// Build a compact, source-backed context pack for a task.
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    /// Inspect registered local models and route task-specific inference.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
}

/// Database commands.
#[derive(Debug, Subcommand)]
enum DatabaseCommand {
    /// Apply pending forward-only migrations.
    Migrate,
    /// Check connectivity and show the latest applied migration.
    Status,
}

/// Deterministic local indexing commands.
#[derive(Debug, Subcommand)]
enum IndexCommand {
    /// Refresh repositories beneath configured roots or explicit paths.
    Refresh {
        /// Root to scan; may be provided more than once.
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
    },
    /// Force a full rebuild of supported content and derived metadata.
    Rescan {
        /// Root to scan; may be provided more than once.
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
    },
    /// Show recent persisted indexing runs and failures.
    Status {
        #[arg(long, default_value_t = 12)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// List indexed projects, optionally filtering by slug and limiting results.
    List {
        /// Only include this project slug.
        #[arg(long)]
        project: Option<String>,
        /// Maximum number of projects to return.
        #[arg(long)]
        limit: Option<usize>,
        /// Return structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// List repository locations for one indexed project.
    Inspect {
        /// Project slug from `project list`.
        project: String,
        /// Return structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// Retrieve a combined project overview, plan, decisions, and checkpoints.
    Context {
        /// Project slug from `project list`.
        project: String,
        /// Return structured JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// List indexed repositories, optionally filtering by project and dirty state.
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, conflicts_with = "clean")]
        dirty: bool,
        #[arg(long)]
        clean: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect a repository by name, root path, or remote.
    Inspect {
        repository: String,
        #[arg(long)]
        json: bool,
    },
    /// List indexed local worktree changes.
    Changes {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PlanCommand {
    /// Retrieve the current indexed plan for a project.
    Current {
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// Synchronize an indexed Markdown plan and its checkboxes.
    Sync {
        project: String,
        /// Relative path of the indexed plan document.
        #[arg(long)]
        path: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// Create a project task.
    Add {
        project: String,
        title: String,
        #[arg(long)]
        description: Option<String>,
        /// Ordinal of a phase in the current plan; omit for an independent task.
        #[arg(long)]
        phase: Option<i32>,
        #[arg(long)]
        json: bool,
    },
    /// List project tasks.
    List {
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// Change a task's status.
    Update {
        task_id: uuid::Uuid,
        #[arg(long, value_enum)]
        status: TaskStatusArg,
        #[arg(long)]
        json: bool,
    },
    /// Set the focused task for a project.
    Focus {
        project: String,
        task_id: uuid::Uuid,
    },
    /// Link a commit, pull request, or implementation reference to a task.
    Link {
        task_id: uuid::Uuid,
        #[arg(long, value_enum)]
        kind: TaskReferenceKindArg,
        reference: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        repository: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum WorkflowCommand {
    /// Retrieve combined workflow state for an agent handoff.
    State {
        project: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum CheckpointCommand {
    /// Record a project checkpoint and repository snapshot.
    Record {
        project: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        summary: String,
        #[arg(long, default_value = "")]
        next_steps: String,
        #[arg(long, default_value = "")]
        blockers: String,
        #[arg(long)]
        task_id: Option<uuid::Uuid>,
        #[arg(long)]
        session_id: Option<uuid::Uuid>,
        /// Return the checkpoint identifier as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DecisionCommand {
    /// Record a project decision and optional source.
    Record {
        project: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        rationale: String,
        #[arg(long)]
        source_path: Option<String>,
        #[arg(long)]
        supersedes: Option<uuid::Uuid>,
        /// Return the decision identifier as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Start a project session.
    Start {
        project: String,
        #[arg(long, default_value = "agent")]
        client_kind: String,
        #[arg(long)]
        task_id: Option<uuid::Uuid>,
    },
    /// Finish an existing session.
    Finish {
        session_id: uuid::Uuid,
        #[arg(long)]
        summary: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TaskStatusArg {
    Pending,
    #[value(name = "in-progress")]
    InProgress,
    Blocked,
    Done,
    Cancelled,
}

impl TaskStatusArg {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TaskReferenceKindArg {
    Commit,
    #[value(name = "pull-request")]
    PullRequest,
    Implementation,
}

impl TaskReferenceKindArg {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::PullRequest => "pull-request",
            Self::Implementation => "implementation",
        }
    }
}

#[derive(Debug, Subcommand)]
enum ContextCommand {
    /// Build a relevance-ranked, byte-bounded context pack for a task.
    Build {
        /// Project slug from `project list`.
        project: String,
        /// Task or question used to select relevant context.
        #[arg(long)]
        task: String,
        /// Return structured JSON.
        #[arg(long)]
        json: bool,
        /// Maximum bytes of source excerpts to include.
        #[arg(long, default_value_t = 12_000)]
        max_bytes: usize,
    },
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    /// List configured and dynamically discovered model cards and task-profile assignments.
    List {
        #[arg(long)]
        allow_hosted: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect one configured or dynamically discovered model profile.
    Profile {
        model_id: String,
        #[arg(long)]
        allow_hosted: bool,
        #[arg(long)]
        json: bool,
    },
    /// Route a prompt through a task profile, with an optional catalog model override.
    Run {
        /// Task profile, such as ask-explainer or architecture-analyst.
        #[arg(long, default_value = "ask-explainer")]
        profile: String,
        /// Prompt to send to the model.
        prompt: String,
        /// Add indexed context from this project slug.
        #[arg(long)]
        project: Option<String>,
        /// Override the profile's model with a catalog model ID.
        #[arg(long)]
        model_id: Option<String>,
        /// Additional context to send with the prompt.
        #[arg(long)]
        context: Option<String>,
        /// Permit hosted inference when the operator also allows it.
        #[arg(long)]
        allow_hosted: bool,
        /// Return structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// Suggest focused tests for supplied code using the test-designer profile.
    GenerateTests {
        /// Source code to analyze.
        code: String,
        /// Add indexed context from this project slug.
        #[arg(long)]
        project: Option<String>,
        /// Permit hosted inference when the operator also allows it.
        #[arg(long)]
        allow_hosted: bool,
        /// Return structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// Compare two catalog models on the same prompt and context.
    Compare {
        first_model_id: String,
        second_model_id: String,
        prompt: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        context: Option<String>,
        #[arg(long)]
        allow_hosted: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show measured calls, failures, latency, tokens, and estimated costs.
    Usage {
        #[arg(long, default_value_t = 50)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FreshnessArg {
    Current,
    Stale,
    Historical,
    Temporary,
    Duplicate,
    Superseded,
}

impl FreshnessArg {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Historical => "historical",
            Self::Temporary => "temporary",
            Self::Duplicate => "duplicate",
            Self::Superseded => "superseded",
        }
    }
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    let config_dir =
        itc_memory_core::load_environment().context("load environment configuration")?;
    let cli = Cli::parse();
    let config = DatabaseConfig::from_environment().context("load database configuration")?;
    let pool = itc_memory_db::connect(&config)
        .await
        .context("connect to PostgreSQL")?;
    match cli.command {
        Command::Database {
            command: DatabaseCommand::Migrate,
        } => {
            itc_memory_db::migrate(&pool)
                .await
                .context("apply database migrations")?;
            println!("Database migrations are current.");
        }
        Command::Database {
            command: DatabaseCommand::Status,
        } => {
            itc_memory_db::check_connection(&pool)
                .await
                .context("check PostgreSQL connection")?;
            match itc_memory_db::latest_migration_version(&pool).await? {
                Some(version) => println!("PostgreSQL is reachable; latest migration: {version}."),
                None => println!("PostgreSQL is reachable; no migrations have been applied."),
            }
        }
        Command::Index {
            command: IndexCommand::Refresh { roots },
        } => {
            let roots = if roots.is_empty() {
                configured_roots(config_dir.as_deref())?
            } else {
                validate_repository_roots(roots)?
            };
            let summary = itc_memory_index::refresh(&pool, &roots)
                .await
                .context("refresh local index")?;
            println!(
                "Indexed {} changed files in {} repositories across {} runs.",
                summary.indexed_files, summary.repositories, summary.runs
            );
            for failure in summary.failures {
                eprintln!("index warning: {failure}");
            }
        }
        Command::Index {
            command: IndexCommand::Rescan { roots },
        } => {
            let roots = if roots.is_empty() {
                configured_roots(config_dir.as_deref())?
            } else {
                validate_repository_roots(roots)?
            };
            let summary = itc_memory_index::rescan(&pool, &roots)
                .await
                .context("rescan local index")?;
            println!(
                "Rescanned {} files in {} repositories across {} runs.",
                summary.indexed_files, summary.repositories, summary.runs
            );
            for failure in summary.failures {
                eprintln!("index warning: {failure}");
            }
        }
        Command::Index {
            command: IndexCommand::Status { limit, json },
        } => {
            let runs = itc_memory_db::indexing_runs(&pool, limit).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&runs)?);
            } else {
                for run in runs {
                    println!(
                        "{}  status={} discovered={} indexed={} failures={} started={}{}",
                        run.root_path,
                        run.status,
                        run.discovered_count,
                        run.indexed_count,
                        run.failed_count,
                        run.started_at,
                        run.error_summary
                            .map_or_else(String::new, |error| format!("\n  {error}"))
                    );
                }
            }
        }
        Command::Project {
            command:
                ProjectCommand::List {
                    project,
                    limit,
                    json,
                },
        } => {
            if let Some(limit) = limit {
                anyhow::ensure!(
                    (1..=100).contains(&limit),
                    "limit must be from 1 through 100"
                );
            }
            let results = itc_memory_db::projects(&pool)
                .await?
                .into_iter()
                .filter(|item| project.as_ref().is_none_or(|slug| item.slug == *slug))
                .take(limit.unwrap_or(usize::MAX))
                .collect::<Vec<_>>();
            print_projects(&results, json)?;
        }
        Command::Project {
            command: ProjectCommand::Inspect { project, json },
        } => {
            let results = itc_memory_db::repositories(&pool, Some(&project), None).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in results {
                    println!(
                        "{}  {}  branch={} head={} dirty={}\n  {}",
                        result.display_name,
                        result
                            .canonical_remote
                            .unwrap_or_else(|| "local-only".to_owned()),
                        result
                            .current_branch
                            .filter(|branch| !branch.is_empty())
                            .unwrap_or_else(|| "detached".to_owned()),
                        result
                            .head_sha
                            .as_deref()
                            .filter(|head| !head.is_empty())
                            .unwrap_or("unknown"),
                        result.is_dirty,
                        result.root_path
                    );
                }
            }
        }
        Command::Project {
            command: ProjectCommand::Context { project, json },
        } => {
            let roots = configured_root_strings(config_dir.as_deref())?;
            let summary = itc_memory_db::projects_under_roots(&pool, &roots)
                .await?
                .into_iter()
                .find(|item| item.slug == project)
                .with_context(|| {
                    format!("project {project:?} is not available in configured checkouts")
                })?;
            let (repositories, plan, decisions, checkpoints) = tokio::try_join!(
                itc_memory_db::repositories_under_roots(&pool, Some(&project), None, &roots),
                itc_memory_db::current_plan_under_roots(&pool, &project, &roots),
                itc_memory_db::decisions_under_roots(&pool, &project, 20, &roots),
                itc_memory_db::checkpoints(&pool, &project, 5),
            )?;
            let context = ProjectContext {
                project: summary,
                repositories,
                plan,
                decisions,
                checkpoints: checkpoints.into_iter().map(Into::into).collect(),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&context)?);
            } else {
                println!(
                    "Project: {} ({})",
                    context.project.display_name, context.project.slug
                );
                print_repositories(&context.repositories, false)?;
                println!("Plans:");
                print_results(&context.plan, false)?;
                for decision in &context.decisions {
                    println!("Decision: {} - {}", decision.title, decision.rationale);
                }
                for checkpoint in &context.checkpoints {
                    println!("Checkpoint: {} - {}", checkpoint.title, checkpoint.summary);
                }
            }
        }
        Command::Repo {
            command:
                RepoCommand::List {
                    project,
                    dirty,
                    clean,
                    json,
                },
        } => {
            let dirty_filter = if dirty {
                Some(true)
            } else if clean {
                Some(false)
            } else {
                None
            };
            let results =
                itc_memory_db::repositories(&pool, project.as_deref(), dirty_filter).await?;
            print_repositories(&results, json)?;
        }
        Command::Repo {
            command: RepoCommand::Inspect { repository, json },
        } => {
            let results = itc_memory_db::repositories(&pool, None, None)
                .await?
                .into_iter()
                .filter(|result| {
                    result.display_name.eq_ignore_ascii_case(&repository)
                        || result.root_path.eq_ignore_ascii_case(&repository)
                        || result
                            .canonical_remote
                            .as_deref()
                            .is_some_and(|remote| remote.eq_ignore_ascii_case(&repository))
                })
                .collect::<Vec<_>>();
            print_repositories(&results, json)?;
        }
        Command::Repo {
            command: RepoCommand::Changes { project, json },
        } => {
            let results = itc_memory_db::repository_changes(&pool, project.as_deref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for change in results {
                    println!(
                        "{}  {}  {}  staged={} unstaged={} untracked={}",
                        change.repository,
                        change.root_path,
                        change.status_text,
                        change.is_staged,
                        change.is_unstaged,
                        change.is_untracked
                    );
                    println!("  {}", change.path);
                }
            }
        }
        Command::Search {
            query,
            project,
            freshness,
            json,
            limit,
        } => {
            let results = itc_memory_db::search(
                &pool,
                &query,
                project.as_deref(),
                freshness.map(FreshnessArg::as_str),
                limit,
            )
            .await?;
            print_results(&results, json)?;
        }
        Command::SearchFiles {
            query,
            project,
            freshness,
            json,
            limit,
        } => {
            anyhow::ensure!(!query.trim().is_empty(), "query cannot be empty");
            anyhow::ensure!(
                !query.contains(['\n', '\r']),
                "search-files requires a single-line literal query"
            );
            anyhow::ensure!(
                (1..=100).contains(&limit),
                "limit must be from 1 through 100"
            );
            let roots = configured_root_strings(config_dir.as_deref())?;
            let results = itc_memory_db::search_file_lines_under_roots(
                &pool,
                &query,
                project.as_deref(),
                freshness.map(FreshnessArg::as_str),
                limit,
                &roots,
            )
            .await?;
            anyhow::ensure!(
                results
                    .iter()
                    .filter_map(|result| result.line.as_ref())
                    .map(String::len)
                    .sum::<usize>()
                    <= 100_000,
                "exact matches exceed the response size limit; narrow the query or limit"
            );
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in results {
                    if let Some(line) = result.line_number {
                        let text = result.line.as_deref().context("matched line is missing")?;
                        println!("{}:{}:{}  {}", result.root_path, result.path, line, text);
                    } else {
                        println!("{}/{}", result.root_path, result.path);
                    }
                }
            }
        }
        Command::SearchCode {
            query,
            project,
            limit,
            json,
        } => {
            anyhow::ensure!(!query.trim().is_empty(), "query cannot be empty");
            anyhow::ensure!(
                (1..=100).contains(&limit),
                "limit must be from 1 through 100"
            );
            let roots = configured_root_strings(config_dir.as_deref())?;
            let (symbols, dependencies, indexed_matches) = tokio::try_join!(
                itc_memory_db::search_symbols_under_roots(
                    &pool,
                    &query,
                    project.as_deref(),
                    limit,
                    &roots
                ),
                itc_memory_db::search_dependencies_under_roots(
                    &pool,
                    &query,
                    project.as_deref(),
                    limit,
                    &roots
                ),
                itc_memory_db::search_under_roots(
                    &pool,
                    &query,
                    project.as_deref(),
                    Some("current"),
                    limit,
                    &roots
                ),
            )?;
            let results = CodeSearchResults {
                symbols,
                dependencies,
                indexed_matches,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for symbol in &results.symbols {
                    println!(
                        "Symbol: {}  {}:{}",
                        symbol.qualified_name, symbol.path, symbol.line_number
                    );
                }
                for dependency in &results.dependencies {
                    println!(
                        "Dependency: {} {}  {}",
                        dependency.dependency_name, dependency.requirement, dependency.root_path
                    );
                }
                print_results(&results.indexed_matches, false)?;
            }
        }
        Command::LintRust {
            project,
            root_path,
            json,
        } => {
            let root = indexed_checkout(&pool, &project, root_path).await?;
            let lint = rust_review::lint(&root, &CancellationToken::new()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&lint)?);
            } else {
                println!(
                    "Clippy success={} exit_code={:?}",
                    lint.success, lint.exit_code
                );
                for diagnostic in &lint.diagnostics {
                    if let Some(rendered) =
                        diagnostic.get("rendered").and_then(|value| value.as_str())
                    {
                        print!("{rendered}");
                    } else {
                        println!("{}", serde_json::to_string(diagnostic)?);
                    }
                }
                if !lint.stderr.is_empty() {
                    eprint!("{}", lint.stderr);
                }
            }
            anyhow::ensure!(lint.success, "Clippy failed");
        }
        Command::ReviewRust {
            project,
            root_path,
            files,
            changed,
            allow_hosted,
            json,
        } => {
            let root = indexed_checkout(&pool, &project, root_path).await?;
            let scope = if changed {
                ReviewScope::Changed
            } else {
                ReviewScope::Files
            };
            let selected_files = (!changed).then_some(files.as_slice());
            let prepared = rust_review::prepare_review(
                &root,
                scope,
                selected_files,
                &CancellationToken::new(),
            )
            .await?;
            let registry = ModelRegistry::from_environment_in(config_dir.as_deref())?;
            let operator_allows_hosted =
                operator_allows_hosted_from_environment().map_err(anyhow::Error::msg)?;
            let context = model_context(
                &pool,
                Some(&project),
                "Rust review and relevant code conventions",
                None,
            )
            .await?;
            let routed = registry
                .run_profile(
                    "rust-reviewer",
                    None,
                    &prepared.prompt,
                    context.as_deref(),
                    allow_hosted,
                    operator_allows_hosted,
                )
                .await;
            record_model_attempts(&pool, &routed.attempts).await?;
            let result = routed.result?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&RustReviewOutput {
                        source: &prepared.source,
                        lint: &prepared.lint,
                        review: ModelRunOutput {
                            result: &result,
                            attempts: &routed.attempts,
                        },
                    })?
                );
            } else {
                println!("Clippy success: {}", prepared.lint.success);
                println!("{}", result.response);
            }
        }
        Command::Symbols {
            query,
            project,
            json,
            limit,
        } => {
            let results =
                itc_memory_db::search_symbols(&pool, &query, project.as_deref(), limit).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in results {
                    println!(
                        "{}::{}  {}:{} ({}, {})",
                        result.project,
                        result.qualified_name,
                        result.path,
                        result.line_number,
                        result.symbol_kind,
                        result.freshness_status
                    );
                }
            }
        }
        Command::Dependencies {
            query,
            project,
            json,
            limit,
        } => {
            let results =
                itc_memory_db::search_dependencies(&pool, &query, project.as_deref(), limit)
                    .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in results {
                    println!(
                        "{}  {} {} ({}, optional={})\n  {}: {}",
                        result.dependency_name,
                        result.requirement,
                        result.dependency_kind,
                        result.target,
                        result.optional,
                        result.repository,
                        result.root_path
                    );
                }
            }
        }
        Command::Plan {
            command: PlanCommand::Current { project, json },
        } => {
            let results = itc_memory_db::current_plan(&pool, &project).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                print_results(&results, false)?;
            }
        }
        Command::Plan {
            command:
                PlanCommand::Sync {
                    project,
                    path,
                    json,
                },
        } => {
            let result = itc_memory_db::sync_plan(&pool, &project, &path).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "Synchronized plan {} ({}) with {} phases and {} checkbox tasks.",
                    result.title, result.plan_id, result.phases, result.tasks
                );
            }
        }
        Command::Task {
            command:
                TaskCommand::Add {
                    project,
                    title,
                    description,
                    phase,
                    json,
                },
        } => {
            let result =
                itc_memory_db::add_task(&pool, &project, &title, description.as_deref(), phase)
                    .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Created task {}: {}", result.id, result.title);
            }
        }
        Command::Task {
            command: TaskCommand::List { project, json },
        } => {
            let results = itc_memory_db::task_list(&pool, &project).await?;
            print_tasks(&results, json)?;
        }
        Command::Task {
            command:
                TaskCommand::Update {
                    task_id,
                    status,
                    json,
                },
        } => {
            let result = itc_memory_db::update_task_status(&pool, task_id, status.as_str()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Updated task {} to {}.", result.id, result.status);
            }
        }
        Command::Task {
            command: TaskCommand::Focus { project, task_id },
        } => {
            itc_memory_db::focus_task(&pool, &project, task_id).await?;
            println!("Focused task {task_id} for project {project}.");
        }
        Command::Task {
            command:
                TaskCommand::Link {
                    task_id,
                    kind,
                    reference,
                    title,
                    url,
                    repository,
                    json,
                },
        } => {
            let result = itc_memory_db::add_task_reference(
                &pool,
                task_id,
                kind.as_str(),
                &reference,
                title.as_deref(),
                url.as_deref(),
                repository.as_deref(),
            )
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "Linked {} {} to task {}.",
                    result.reference_kind, result.reference, result.task_id
                );
            }
        }
        Command::Workflow {
            command: WorkflowCommand::State { project, json },
        } => {
            let result = itc_memory_db::current_workflow_state(&pool, &project).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_workflow_state(&result);
            }
        }
        Command::Checkpoint {
            command:
                CheckpointCommand::Record {
                    project,
                    title,
                    summary,
                    next_steps,
                    blockers,
                    task_id,
                    session_id,
                    json,
                },
        } => {
            let id = itc_memory_db::record_checkpoint(
                &pool,
                itc_memory_db::CheckpointInput {
                    project: &project,
                    title: &title,
                    summary: &summary,
                    next_steps: &next_steps,
                    blockers: &blockers,
                    task_id,
                    session_id,
                },
            )
            .await?;
            if json {
                println!("{}", serde_json::json!({ "checkpoint_id": id }));
            } else {
                println!("Recorded checkpoint {id} for {project}.");
            }
        }
        Command::Decision {
            command:
                DecisionCommand::Record {
                    project,
                    title,
                    rationale,
                    source_path,
                    supersedes,
                    json,
                },
        } => {
            let id = itc_memory_db::record_decision(
                &pool,
                &project,
                &title,
                &rationale,
                source_path.as_deref(),
                supersedes,
            )
            .await?;
            if json {
                println!("{}", serde_json::json!({ "decision_id": id }));
            } else {
                println!("Recorded validated decision {id} for {project}.");
            }
        }
        Command::Session {
            command:
                SessionCommand::Start {
                    project,
                    client_kind,
                    task_id,
                },
        } => {
            let id = itc_memory_db::start_session(&pool, &project, &client_kind, task_id).await?;
            println!("{id}");
        }
        Command::Session {
            command:
                SessionCommand::Finish {
                    session_id,
                    summary,
                },
        } => {
            itc_memory_db::finish_session(&pool, session_id, summary.as_deref()).await?;
            println!("Finished session {session_id}.");
        }
        Command::Decisions {
            project,
            json,
            limit,
        } => {
            let results = itc_memory_db::decisions(&pool, &project, limit).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in results {
                    println!(
                        "{} [{}{}] {}\n  Source: {} (freshness={})",
                        result.title,
                        result.status,
                        if result.is_stale { ", stale" } else { "" },
                        result.rationale,
                        result.source_path.as_deref().unwrap_or("not recorded"),
                        result.source_freshness.as_deref().unwrap_or("unknown")
                    );
                }
            }
        }
        Command::Checkpoints {
            project,
            json,
            limit,
        } => {
            let results = itc_memory_db::checkpoints(&pool, &project, limit).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in results {
                    println!(
                        "{} ({})\n{}\n  next={} blockers={}\n  task={} session={} repository_state={}",
                        result.title,
                        result.created_at,
                        result.summary,
                        result.next_steps,
                        result.blockers,
                        result.task_id.as_deref().unwrap_or("none"),
                        result.session_id.as_deref().unwrap_or("none"),
                        result.repository_state
                    );
                }
            }
        }
        Command::Context {
            command:
                ContextCommand::Build {
                    project,
                    task,
                    json,
                    max_bytes,
                },
        } => {
            anyhow::ensure!(!task.trim().is_empty(), "context task cannot be empty");
            let pack = itc_memory_db::context_pack(&pool, &project, &task, max_bytes).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pack)?);
            } else {
                println!(
                    "Context: {} sources, {} / {} excerpt bytes, average fused relevance {:.3}, retrieval {} us\nTask: {}",
                    pack.metrics.source_count,
                    pack.metrics.context_bytes,
                    pack.metrics.max_context_bytes,
                    pack.metrics.average_score,
                    pack.metrics.retrieval_latency_us,
                    pack.task
                );
                print_results(&pack.sources, false)?;
            }
        }
        Command::Model { command } => match command {
            ModelCommand::List { allow_hosted, json } => {
                let registry = ModelRegistry::from_environment_in(config_dir.as_deref())?;
                let operator_allows_hosted =
                    operator_allows_hosted_from_environment().map_err(anyhow::Error::msg)?;
                let cards = registry
                    .model_cards(allow_hosted, operator_allows_hosted)
                    .await;
                let usage = itc_memory_db::model_usage_summaries(&pool, 100).await?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&ModelCatalogOutput {
                            models: &cards,
                            task_profiles: registry.task_profiles(),
                            usage: &usage,
                        })?
                    );
                } else {
                    print_model_catalog(&cards, registry.task_profiles());
                    println!("Measured usage groups: {}", usage.len());
                }
            }
            ModelCommand::Profile {
                model_id,
                allow_hosted,
                json,
            } => {
                let registry = ModelRegistry::from_environment_in(config_dir.as_deref())?;
                let operator_allows_hosted =
                    operator_allows_hosted_from_environment().map_err(anyhow::Error::msg)?;
                let cards = registry
                    .model_cards(allow_hosted, operator_allows_hosted)
                    .await;
                let card = cards
                    .into_iter()
                    .find(|card| card.model.id == model_id)
                    .with_context(|| {
                        format!("model {model_id:?} is not in the current provider catalogue")
                    })?;
                let task_profiles = registry
                    .task_profiles()
                    .iter()
                    .filter(|profile| {
                        profile.preferred_model_id == model_id
                            || profile.fallback_model_ids.contains(&model_id)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let usage = itc_memory_db::model_usage_summaries(&pool, 100)
                    .await?
                    .into_iter()
                    .filter(|item| item.model_id == model_id)
                    .collect::<Vec<_>>();
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&ModelProfileOutput {
                            card: &card,
                            task_profiles: &task_profiles,
                            usage: &usage,
                        })?
                    );
                } else {
                    print_model_catalog(std::slice::from_ref(&card), &task_profiles);
                    for item in usage {
                        println!(
                            "usage profile={} scope={} calls={} failures={} avg_ms={} input_tokens={} output_tokens={} estimated_cost_usd={}",
                            item.task_profile_id.as_deref().unwrap_or("none"),
                            provider_scope(item.hosted, item.trusted_lan),
                            item.call_count,
                            item.failure_count,
                            item.average_latency_ms,
                            item.input_tokens
                                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                            item.output_tokens
                                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                            item.estimated_cost_usd.map_or_else(
                                || "unknown".to_owned(),
                                |value| format!("{value:.6}")
                            )
                        );
                    }
                }
            }
            ModelCommand::Run {
                profile,
                prompt,
                project,
                model_id,
                context,
                allow_hosted,
                json,
            } => {
                anyhow::ensure!(!prompt.trim().is_empty(), "prompt cannot be empty");
                let registry = ModelRegistry::from_environment_in(config_dir.as_deref())?;
                let operator_allows_hosted =
                    operator_allows_hosted_from_environment().map_err(anyhow::Error::msg)?;
                let context =
                    model_context(&pool, project.as_deref(), &prompt, context.as_deref()).await?;
                let routed = registry
                    .run_profile(
                        &profile,
                        model_id.as_deref(),
                        &prompt,
                        context.as_deref(),
                        allow_hosted,
                        operator_allows_hosted,
                    )
                    .await;
                record_model_attempts(&pool, &routed.attempts).await?;
                print_model_run(routed, json)?;
            }
            ModelCommand::GenerateTests {
                code,
                project,
                allow_hosted,
                json,
            } => {
                anyhow::ensure!(!code.trim().is_empty(), "code cannot be empty");
                let registry = ModelRegistry::from_environment_in(config_dir.as_deref())?;
                let operator_allows_hosted =
                    operator_allows_hosted_from_environment().map_err(anyhow::Error::msg)?;
                let context = model_context(
                    &pool,
                    project.as_deref(),
                    "test conventions and coverage for this source",
                    None,
                )
                .await?;
                let prompt = format!(
                    "Suggest focused tests for this code. Do not claim they were run:\n```\n{code}\n```"
                );
                let routed = registry
                    .run_profile(
                        "test-designer",
                        None,
                        &prompt,
                        context.as_deref(),
                        allow_hosted,
                        operator_allows_hosted,
                    )
                    .await;
                record_model_attempts(&pool, &routed.attempts).await?;
                print_model_run(routed, json)?;
            }
            ModelCommand::Compare {
                first_model_id,
                second_model_id,
                prompt,
                project,
                context,
                allow_hosted,
                json,
            } => {
                anyhow::ensure!(!prompt.trim().is_empty(), "prompt cannot be empty");
                anyhow::ensure!(
                    first_model_id != second_model_id,
                    "comparison requires two different selectable models"
                );
                let registry = ModelRegistry::from_environment_in(config_dir.as_deref())?;
                let operator_allows_hosted =
                    operator_allows_hosted_from_environment().map_err(anyhow::Error::msg)?;
                let context =
                    model_context(&pool, project.as_deref(), &prompt, context.as_deref()).await?;
                let (first, second) = tokio::join!(
                    registry.run_registered_model(ExplicitModelRequest {
                        model_id: &first_model_id,
                        task_profile_id: None,
                        prompt: &prompt,
                        context: context.as_deref(),
                        allow_hosted,
                        operator_allows_hosted,
                        fallback_used: false,
                    }),
                    registry.run_registered_model(ExplicitModelRequest {
                        model_id: &second_model_id,
                        task_profile_id: None,
                        prompt: &prompt,
                        context: context.as_deref(),
                        allow_hosted,
                        operator_allows_hosted,
                        fallback_used: false,
                    })
                );
                record_model_attempt(&pool, first.attempt.as_ref()).await?;
                record_model_attempt(&pool, second.attempt.as_ref()).await?;
                let comparison = ModelComparisonOutput {
                    first: comparison_entry(first_model_id, first),
                    second: comparison_entry(second_model_id, second),
                };
                let failed = comparison.first.error.is_some() || comparison.second.error.is_some();
                if json {
                    println!("{}", serde_json::to_string_pretty(&comparison)?);
                } else {
                    print_comparison_entry(&comparison.first);
                    print_comparison_entry(&comparison.second);
                }
                anyhow::ensure!(!failed, "one or more model comparisons failed");
            }
            ModelCommand::Usage { limit, json } => {
                let usage = itc_memory_db::model_usage_summaries(&pool, limit).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&usage)?);
                } else if usage.is_empty() {
                    println!("No model inference usage has been recorded.");
                } else {
                    for item in usage {
                        println!(
                            "{} profile={} provider={} scope={} calls={} failures={} avg_ms={} input_tokens={} output_tokens={} estimated_cost_usd={}",
                            item.model_id,
                            item.task_profile_id.as_deref().unwrap_or("none"),
                            item.provider_id,
                            provider_scope(item.hosted, item.trusted_lan),
                            item.call_count,
                            item.failure_count,
                            item.average_latency_ms,
                            item.input_tokens
                                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                            item.output_tokens
                                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                            item.estimated_cost_usd.map_or_else(
                                || "unknown".to_owned(),
                                |value| format!("{value:.6}")
                            )
                        );
                    }
                }
            }
        },
    }
    Ok(())
}

#[derive(Serialize)]
struct ProjectContext {
    project: itc_memory_db::ProjectSummary,
    repositories: Vec<itc_memory_db::RepositorySummary>,
    plan: Vec<itc_memory_db::SearchResult>,
    decisions: Vec<itc_memory_db::DecisionResult>,
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

impl From<itc_memory_db::CheckpointResult> for ContextCheckpoint {
    fn from(checkpoint: itc_memory_db::CheckpointResult) -> Self {
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
    symbols: Vec<itc_memory_db::SymbolResult>,
    dependencies: Vec<itc_memory_db::DependencyResult>,
    indexed_matches: Vec<itc_memory_db::SearchResult>,
}

#[derive(Serialize)]
struct ModelCatalogOutput<'a> {
    models: &'a [ModelCard],
    task_profiles: &'a [ModelTaskProfile],
    usage: &'a [itc_memory_db::ModelUsageSummary],
}

#[derive(Serialize)]
struct ModelProfileOutput<'a> {
    card: &'a ModelCard,
    task_profiles: &'a [ModelTaskProfile],
    usage: &'a [itc_memory_db::ModelUsageSummary],
}

#[derive(Serialize)]
struct ModelRunOutput<'a> {
    result: &'a itc_memory_model::InferenceResult,
    attempts: &'a [InferenceAttempt],
}

#[derive(Serialize)]
struct RustReviewOutput<'a> {
    source: &'a ReviewSource,
    lint: &'a LintResult,
    review: ModelRunOutput<'a>,
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

fn print_model_catalog(cards: &[ModelCard], profiles: &[ModelTaskProfile]) {
    for card in cards {
        let availability = match &card.availability {
            Availability::Available => "available".to_owned(),
            Availability::Unavailable { reason } => format!("unavailable: {reason}"),
            Availability::HostedConsentRequired => "hosted consent required".to_owned(),
        };
        println!(
            "{}  {}  provider={} model={} scope={} source={} status={}",
            card.model.id,
            card.model.display_name,
            card.provider.id,
            card.model.model_identifier,
            provider_scope(card.provider.hosted, card.provider.trusted_lan),
            if card.discovered {
                "discovered"
            } else {
                "configured"
            },
            availability
        );
        println!(
            "  roles={} capabilities={} context_window={}",
            card.model.supported_task_profiles.join(","),
            card.model
                .capabilities
                .iter()
                .map(|capability| format!("{capability:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(","),
            card.model
                .context_window
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
        );
        for limitation in &card.model.known_limitations {
            println!("  limitation: {limitation}");
        }
        for evaluation in &card.model.evaluation_results {
            println!("  evaluation: {evaluation}");
        }
    }
    for profile in profiles {
        println!(
            "task={} preferred={} fallback={}",
            profile.id,
            profile.preferred_model_id,
            if profile.fallback_model_ids.is_empty() {
                "none".to_owned()
            } else {
                profile.fallback_model_ids.join(",")
            }
        );
        println!("  {}", profile.responsibility);
    }
}

const fn provider_scope(hosted: bool, trusted_lan: bool) -> &'static str {
    if hosted {
        "hosted"
    } else if trusted_lan {
        "trusted_lan"
    } else {
        "local"
    }
}

fn comparison_entry(model_id: String, call: ModelCall) -> ModelComparisonEntry {
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

fn print_model_run(routed: RoutedInference, json: bool) -> anyhow::Result<()> {
    match routed.result {
        Ok(result) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ModelRunOutput {
                        result: &result,
                        attempts: &routed.attempts,
                    })?
                );
            } else {
                println!(
                    "model={} profile={} fallback={} latency_ms={} input_tokens={} output_tokens={}",
                    result.model_id,
                    result.task_profile_id.as_deref().unwrap_or("none"),
                    result.attempt.fallback_used,
                    result.attempt.latency_ms,
                    result
                        .attempt
                        .input_tokens
                        .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                    result
                        .attempt
                        .output_tokens
                        .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
                );
                println!("{}", result.response);
            }
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "error": error.to_string(),
                        "attempts": routed.attempts
                    }))?
                );
            }
            return Err(error.into());
        }
    }
    Ok(())
}

fn print_comparison_entry(entry: &ModelComparisonEntry) {
    println!("Model: {}", entry.model_id);
    if let Some(response) = &entry.response {
        println!("{response}");
    }
    if let Some(error) = &entry.error {
        println!("Error: {error}");
    }
    if let Some(attempt) = &entry.attempt {
        println!(
            "latency_ms={} input_tokens={} output_tokens={} estimated_cost_usd={}",
            attempt.latency_ms,
            attempt
                .input_tokens
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
            attempt
                .output_tokens
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
            attempt
                .estimated_cost_usd
                .map_or_else(|| "unknown".to_owned(), |value| format!("{value:.6}"))
        );
    }
}

async fn model_context(
    pool: &sqlx::PgPool,
    project: Option<&str>,
    task: &str,
    extra: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let mut sections = Vec::new();
    if let Some(extra) = extra.filter(|value| !value.trim().is_empty()) {
        sections.push(extra.to_owned());
    }
    if let Some(project) = project {
        let pack = itc_memory_db::context_pack(pool, project, task, 16_000).await?;
        sections.push(serde_json::to_string(&pack)?);
    }
    Ok((!sections.is_empty()).then(|| sections.join("\n\n")))
}

async fn record_model_attempts(
    pool: &sqlx::PgPool,
    attempts: &[InferenceAttempt],
) -> anyhow::Result<()> {
    for attempt in attempts {
        record_model_attempt(pool, Some(attempt)).await?;
    }
    Ok(())
}

async fn record_model_attempt(
    pool: &sqlx::PgPool,
    attempt: Option<&InferenceAttempt>,
) -> anyhow::Result<()> {
    let Some(attempt) = attempt else {
        return Ok(());
    };
    let to_i64 = |value: u64, field: &str| {
        i64::try_from(value).with_context(|| format!("{field} is out of range"))
    };
    itc_memory_db::record_model_inference(
        pool,
        &itc_memory_db::ModelInferenceInput {
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
            latency_ms: to_i64(attempt.latency_ms, "latency")?,
            input_tokens: attempt
                .input_tokens
                .map(|value| to_i64(value, "input token count"))
                .transpose()?,
            output_tokens: attempt
                .output_tokens
                .map(|value| to_i64(value, "output token count"))
                .transpose()?,
            estimated_cost_usd: attempt.estimated_cost_usd,
        },
    )
    .await?;
    Ok(())
}

fn print_tasks(results: &[itc_memory_db::TaskResult], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(results)?);
    } else {
        for task in results {
            println!(
                "{} [{}] {}{}",
                task.id,
                task.status,
                task.title,
                task.phase_ordinal
                    .map_or_else(String::new, |ordinal| format!(" (phase {ordinal})"))
            );
            if let (Some(path), Some(line)) = (&task.source_path, task.source_line) {
                println!(
                    "  source: {path}:{line} checkbox_checked={}",
                    task.checkbox_checked
                );
            }
            if let Some(description) = &task.description {
                println!("  {description}");
            }
        }
    }
    Ok(())
}

fn print_workflow_state(state: &itc_memory_db::WorkflowState) {
    println!("Project: {}", state.project);
    if let Some(plan) = &state.plan {
        println!(
            "Plan: {} [{}{}]{}",
            plan.title,
            plan.status,
            if plan.is_stale { ", stale" } else { "" },
            plan.source_path
                .as_deref()
                .map_or_else(String::new, |path| format!(" ({path})"))
        );
    } else {
        println!("Plan: none indexed");
    }
    if let Some(task) = &state.current_task {
        println!("Current task: {} [{}] {}", task.id, task.status, task.title);
    } else {
        println!("Current task: none focused");
    }
    for phase in &state.phases {
        println!(
            "Phase {} [{}] {} ({} / {} tasks done, {} checked)",
            phase.ordinal,
            phase.status,
            phase.title,
            phase.completed_count,
            phase.task_count,
            phase.checked_count
        );
    }
    for task in &state.tasks {
        println!("Task {} [{}] {}", task.id, task.status, task.title);
    }
    for decision in &state.decisions {
        println!(
            "Decision {} [{}{}] {} — {}",
            decision.id,
            decision.status,
            if decision.is_stale { ", stale" } else { "" },
            decision.title,
            decision.rationale
        );
    }
    for checkpoint in &state.checkpoints {
        println!(
            "Checkpoint {}: {}\n  {}\n  next: {}\n  blockers: {}",
            checkpoint.created_at,
            checkpoint.title,
            checkpoint.summary,
            checkpoint.next_steps,
            checkpoint.blockers
        );
    }
    for change in &state.changes_since_checkpoint {
        println!(
            "Changed since checkpoint: {} {}",
            change.status_text, change.path
        );
    }
    for reference in &state.task_references {
        println!(
            "Task reference {} [{}]: {}{}",
            reference.task_id,
            reference.reference_kind,
            reference.reference,
            reference
                .url
                .as_deref()
                .map_or_else(String::new, |url| format!(" ({url})"))
        );
    }
}

fn print_repositories(
    results: &[itc_memory_db::RepositorySummary],
    json: bool,
) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(results)?);
    } else {
        for result in results {
            println!(
                "{}  {}  branch={} head={} dirty={}\n  {}",
                result.display_name,
                result.canonical_remote.as_deref().unwrap_or("local-only"),
                result
                    .current_branch
                    .as_deref()
                    .filter(|branch| !branch.is_empty())
                    .unwrap_or("detached"),
                result
                    .head_sha
                    .as_deref()
                    .filter(|head| !head.is_empty())
                    .unwrap_or("unknown"),
                result.is_dirty,
                result.root_path
            );
        }
    }
    Ok(())
}

fn print_projects(results: &[itc_memory_db::ProjectSummary], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(results)?);
    } else {
        for result in results {
            println!(
                "{}  repos={} documents={}",
                result.slug, result.repositories, result.documents
            );
        }
    }
    Ok(())
}
fn print_results(results: &[itc_memory_db::SearchResult], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(results)?);
    } else {
        for result in results {
            println!(
                "[{}] {} ({}, score {:.3}, seen {}, freshness {})\n{}\n{}\n",
                result.project,
                result.path,
                result.document_kind,
                result.score,
                result.last_seen_at,
                result.freshness_status,
                result.reason,
                result.excerpt
            );
        }
    }
    Ok(())
}

async fn indexed_checkout(
    pool: &sqlx::PgPool,
    project: &str,
    root_path: PathBuf,
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(!project.trim().is_empty(), "project cannot be empty");
    let root = validate_repository_roots(vec![root_path])?
        .into_iter()
        .next()
        .context("missing checkout root")?;
    let repositories = itc_memory_db::repositories(pool, Some(project), None).await?;
    anyhow::ensure!(
        repositories
            .iter()
            .any(|repository| Path::new(&repository.root_path) == root),
        "root_path is not an indexed checkout of the requested project"
    );
    Ok(root)
}

fn configured_roots(config_dir: Option<&Path>) -> anyhow::Result<Vec<PathBuf>> {
    match std::env::var("ITC_MEMORY_INDEX_ROOTS_FILE") {
        Ok(path) => {
            let path = itc_memory_core::resolve_config_path(path, config_dir);
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("read configured index roots file {}", path.display()))?;
            return parse_root_file(&contents);
        }

        Err(std::env::VarError::NotPresent) => {}
        Err(error) => return Err(error.into()),
    }
    let configured = match std::env::var("ITC_MEMORY_INDEX_ROOTS") {
        Ok(configured) => configured,
        Err(std::env::VarError::NotPresent) => {
            if itc_memory_core::EMBEDDED_INDEX_ROOTS.is_empty() {
                std::env::current_dir()
                    .context("resolve the current folder for the default index root")?
                    .to_string_lossy()
                    .into_owned()
            } else {
                return parse_root_file(itc_memory_core::EMBEDDED_INDEX_ROOTS);
            }
        }
        Err(error) => return Err(error.into()),
    };
    parse_configured_roots(&configured)
}

fn configured_root_strings(config_dir: Option<&Path>) -> anyhow::Result<Vec<String>> {
    Ok(configured_roots(config_dir)?
        .into_iter()
        .map(|root| root.to_string_lossy().into_owned())
        .collect())
}

fn parse_root_file(contents: &str) -> anyhow::Result<Vec<PathBuf>> {
    let roots = contents
        .lines()
        .map(str::trim)
        .filter(|root| !root.is_empty() && !root.starts_with('#'))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if roots.is_empty() {
        anyhow::bail!("index roots file must list at least one exact Git checkout");
    }
    validate_repository_roots(roots)
}

fn parse_configured_roots(configured: &str) -> anyhow::Result<Vec<PathBuf>> {
    let roots = configured
        .split(',')
        .map(|raw_root| {
            let raw_root = raw_root.trim();
            if raw_root.is_empty() {
                anyhow::bail!("ITC_MEMORY_INDEX_ROOTS contains an empty folder");
            }
            Ok(PathBuf::from(raw_root))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    validate_repository_roots(roots)
}

fn validate_repository_roots(roots: Vec<PathBuf>) -> anyhow::Result<Vec<PathBuf>> {
    let mut validated_roots = Vec::new();
    for root in roots {
        if !root.is_absolute() || root == Path::new("/") {
            anyhow::bail!(
                "index folder must be an absolute, non-root path: {}",
                root.display()
            );
        }
        let root = std::fs::canonicalize(&root)
            .with_context(|| format!("resolve repository folder {}", root.display()))?;
        if !root.join(".git").exists() {
            anyhow::bail!(
                "index folder must be an exact Git checkout containing .git; refusing to scan descendants: {}",
                root.display()
            );
        }
        if !validated_roots.contains(&root) {
            validated_roots.push(root);
        }
    }
    if validated_roots.is_empty() {
        anyhow::bail!("ITC_MEMORY_INDEX_ROOTS must contain at least one repository folder");
    }
    Ok(validated_roots)
}

#[cfg(test)]
mod tests {
    use super::{Cli, parse_configured_roots, parse_root_file};
    use clap::{CommandFactory, Parser};
    use std::{fs, time::SystemTime};

    #[test]
    fn every_command_has_discoverable_help() {
        fn check(command: &clap::Command, path: &str) {
            for subcommand in command.get_subcommands() {
                let full_path = format!("{path} {}", subcommand.get_name());
                assert!(
                    subcommand
                        .get_about()
                        .is_some_and(|about| !about.to_string().trim().is_empty()),
                    "{full_path} has no help description"
                );
                check(subcommand, &full_path);
            }
        }
        check(&Cli::command(), "itc-memory");
    }

    #[test]
    fn rust_review_requires_a_single_source_scope() {
        let base = [
            "itc-memory",
            "review-rust",
            "project",
            "--root-path",
            "/repo",
        ];
        assert!(Cli::try_parse_from(base).is_err());
        assert!(Cli::try_parse_from(base.into_iter().chain(["--changed"])).is_ok());
        assert!(Cli::try_parse_from(base.into_iter().chain(["--file", "src/lib.rs"])).is_ok());
        assert!(
            Cli::try_parse_from(
                base.into_iter()
                    .chain(["--changed", "--file", "src/lib.rs"])
            )
            .is_err()
        );
    }

    #[test]
    fn configured_roots_accept_only_explicit_checkout_folders() {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock after Unix epoch")
            .as_nanos();
        let parent = std::env::temp_dir().join(format!("itc-memory-root-test-{nonce}"));
        let first = parent.join("repo-a");
        let second = parent.join("repo-b");
        fs::create_dir_all(first.join(".git")).expect("create first checkout");
        fs::create_dir_all(second.join(".git")).expect("create second checkout");

        assert!(parse_configured_roots(&parent.to_string_lossy()).is_err());
        let roots = parse_configured_roots(&format!("{},{}", first.display(), second.display()))
            .expect("parse explicit repository allowlist");
        assert_eq!(roots, vec![first.clone(), second.clone()]);
        let file_roots = parse_root_file(&format!(
            "# curated checkout list\n{}\n\n{}\n",
            first.display(),
            second.display()
        ))
        .expect("parse line-based repository allowlist");
        assert_eq!(file_roots, roots);

        fs::remove_dir_all(parent).expect("remove only the test directory");
    }
}
