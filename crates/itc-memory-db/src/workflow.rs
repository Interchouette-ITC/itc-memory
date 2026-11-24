//! Structured project workflow operations.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    CheckpointResult, DecisionResult, RepositoryChange, checkpoints, current_plan, decisions,
    repositories, repositories_under_roots, repository_changes, repository_changes_under_roots,
};

/// Workflow operation failures with explicit validation and not-found messages.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum WorkflowError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("{0} was not found in the requested project")]
    NotFound(&'static str),
    #[error("invalid task status: {0}")]
    InvalidTaskStatus(String),
    #[error("invalid task reference kind: {0}")]
    InvalidReferenceKind(String),
    #[error("{0} cannot be empty")]
    EmptyField(&'static str),
    #[error("invalid checkpoint repository snapshot: {0}")]
    InvalidCheckpointState(String),
}

/// Summary returned after importing a plan document and its checkboxes.
#[derive(Debug, Serialize)]
pub struct PlanSyncSummary {
    /// Persistent plan identifier.
    pub plan_id: String,
    /// Imported title.
    pub title: String,
    /// Number of imported phases.
    pub phases: usize,
    /// Number of imported checkbox tasks.
    pub tasks: usize,
}

/// Current normalized plan metadata.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct WorkflowPlan {
    /// Persistent plan identifier, absent when only an indexed file is available.
    pub id: Option<String>,
    /// Plan title.
    pub title: String,
    /// Relative indexed source path, when available.
    pub source_path: Option<String>,
    /// Plan status.
    pub status: String,
    /// Whether the source has changed since the plan was synchronized.
    pub is_stale: bool,
    /// Last update timestamp.
    pub updated_at: Option<String>,
}

/// One phase and its aggregate task progress.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct WorkflowPhase {
    /// Persistent phase identifier.
    pub id: String,
    /// Plan ordinal.
    pub ordinal: i32,
    /// Phase title.
    pub title: String,
    /// Current phase status.
    pub status: String,
    /// Number of non-cancelled tasks in the phase.
    pub task_count: i64,
    /// Number of completed tasks in the phase.
    pub completed_count: i64,
    /// Number of tasks checked in the source plan.
    pub checked_count: i64,
}

/// One project task, including its plan checkbox state when imported.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TaskResult {
    /// Persistent task identifier.
    pub id: String,
    /// Optional containing phase ordinal.
    pub phase_ordinal: Option<i32>,
    /// Optional containing phase title.
    pub phase_title: Option<String>,
    /// Task title.
    pub title: String,
    /// Optional task description.
    pub description: Option<String>,
    /// Workflow status.
    pub status: String,
    /// Whether the source plan checkbox was checked at last synchronization.
    pub checkbox_checked: bool,
    /// Indexed source path for plan-derived tasks.
    pub source_path: Option<String>,
    /// Source line for plan-derived tasks.
    pub source_line: Option<i32>,
}

/// A commit, pull request, or implementation reference attached to a task.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TaskReferenceResult {
    /// Persistent task identifier.
    pub task_id: String,
    /// Task title.
    pub task_title: String,
    /// Reference kind.
    pub reference_kind: String,
    /// SHA, PR number, or implementation reference.
    pub reference: String,
    /// Optional reference title.
    pub title: Option<String>,
    /// Optional URL.
    pub url: Option<String>,
    /// Optional repository display name.
    pub repository: Option<String>,
}

/// Agent-ready structured project state.
#[derive(Debug, Serialize)]
pub struct WorkflowState {
    /// Project slug.
    pub project: String,
    /// Current normalized plan or the latest indexed plan document.
    pub plan: Option<WorkflowPlan>,
    /// Phases for the current normalized plan.
    pub phases: Vec<WorkflowPhase>,
    /// Current-plan and standalone project tasks.
    pub tasks: Vec<TaskResult>,
    /// Explicitly focused task, if any.
    pub current_task: Option<TaskResult>,
    /// Validated decisions, with source freshness.
    pub decisions: Vec<DecisionResult>,
    /// Recent checkpoints and handoffs.
    pub checkpoints: Vec<CheckpointResult>,
    /// Local changes observed after the latest checkpoint.
    pub changes_since_checkpoint: Vec<RepositoryChange>,
    /// Commit, PR, and implementation references on current tasks.
    pub task_references: Vec<TaskReferenceResult>,
}

/// Inputs for independently recording a checkpoint or handoff.
pub struct CheckpointInput<'a> {
    /// Project slug.
    pub project: &'a str,
    /// Checkpoint title.
    pub title: &'a str,
    /// Progress summary.
    pub summary: &'a str,
    /// Suggested next steps.
    pub next_steps: &'a str,
    /// Current blockers.
    pub blockers: &'a str,
    /// Optional related task.
    pub task_id: Option<Uuid>,
    /// Optional recorded agent session.
    pub session_id: Option<Uuid>,
}

#[derive(Debug)]
struct ParsedPhase {
    ordinal: i32,
    title: String,
    source_line: i32,
    status: String,
}

#[derive(Debug)]
struct ParsedTask {
    phase_ordinal: Option<i32>,
    title: String,
    checked: bool,
    source_line: i32,
}

#[derive(Debug)]
struct ParsedPlan {
    phases: Vec<ParsedPhase>,
    tasks: Vec<ParsedTask>,
}

/// Synchronizes a current indexed Markdown plan into normalized workflow rows.
///
/// The indexed file remains authoritative. Synchronization imports phase headings
/// and checkboxes without editing the project file.
///
/// # Errors
/// Returns an error if the source plan cannot be found or the database rejects the write.
#[allow(clippy::too_many_lines)]
pub async fn sync_plan(
    pool: &PgPool,
    project: &str,
    source_path: &str,
) -> Result<PlanSyncSummary, WorkflowError> {
    let source: Option<(Uuid, Uuid, Uuid, String, Option<String>, String)> =
        sqlx::query_as(query_sql!("workflow/sync_plan_01.sql"))
            .bind(project)
            .bind(source_path)
            .fetch_optional(pool)
            .await?;
    let Some((project_id, document_id, revision_id, path, source_title, content)) = source else {
        return Err(WorkflowError::NotFound("current indexed plan"));
    };

    let parsed = parse_plan(&content);
    let title = markdown_plan_title(&content)
        .or(source_title)
        .unwrap_or_else(|| path.clone());
    let mut transaction = pool.begin().await?;
    sqlx::query(query_sql!("workflow/sync_plan_02.sql"))
        .bind(project_id)
        .bind(document_id)
        .execute(&mut *transaction)
        .await?;
    let plan_id: Uuid = sqlx::query_scalar(query_sql!("workflow/sync_plan_03.sql"))
        .bind(Uuid::new_v4())
        .bind(project_id)
        .bind(document_id)
        .bind(revision_id)
        .bind(&title)
        .fetch_one(&mut *transaction)
        .await?;

    sqlx::query(query_sql!("workflow/sync_plan_04.sql"))
        .bind(plan_id)
        .execute(&mut *transaction)
        .await?;
    let mut phase_ids = HashMap::new();
    for phase in &parsed.phases {
        let id: Uuid = sqlx::query_scalar(query_sql!("workflow/sync_plan_05.sql"))
            .bind(Uuid::new_v4())
            .bind(plan_id)
            .bind(phase.ordinal)
            .bind(&phase.title)
            .bind(&phase.status)
            .bind(phase.source_line)
            .fetch_one(&mut *transaction)
            .await?;
        phase_ids.insert(phase.ordinal, id);
    }

    let mut current_lines = HashSet::new();
    for task in &parsed.tasks {
        current_lines.insert(task.source_line);
        let phase_id = task
            .phase_ordinal
            .and_then(|ordinal| phase_ids.get(&ordinal).copied());
        let status = if task.checked { "done" } else { "pending" };
        let reference = json!({
            "path": path,
            "line": task.source_line,
            "checkbox_checked": task.checked
        });
        let existing: Option<(Uuid, String)> =
            sqlx::query_as(query_sql!("workflow/sync_plan_06.sql"))
                .bind(document_id)
                .bind(task.source_line)
                .fetch_optional(&mut *transaction)
                .await?;
        if let Some((task_id, _)) = existing {
            sqlx::query(query_sql!("workflow/sync_plan_07.sql"))
                .bind(task_id)
                .bind(phase_id)
                .bind(&task.title)
                .bind(status)
                .bind(task.checked)
                .bind(reference)
                .execute(&mut *transaction)
                .await?;
        } else {
            sqlx::query(query_sql!("workflow/sync_plan_08.sql"))
                .bind(Uuid::new_v4())
                .bind(project_id)
                .bind(phase_id)
                .bind(document_id)
                .bind(task.source_line)
                .bind(&task.title)
                .bind(status)
                .bind(task.checked)
                .bind(reference)
                .execute(&mut *transaction)
                .await?;
        }
    }
    let existing_tasks: Vec<(Uuid, i32)> = sqlx::query_as(query_sql!("workflow/sync_plan_09.sql"))
        .bind(document_id)
        .fetch_all(&mut *transaction)
        .await?;
    for (task_id, source_line) in existing_tasks {
        if !current_lines.contains(&source_line) {
            sqlx::query(query_sql!("workflow/sync_plan_10.sql"))
                .bind(task_id)
                .execute(&mut *transaction)
                .await?;
        }
    }
    sqlx::query(query_sql!("workflow/sync_plan_11.sql"))
        .bind(plan_id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;

    Ok(PlanSyncSummary {
        plan_id: plan_id.to_string(),
        title,
        phases: parsed.phases.len(),
        tasks: parsed.tasks.len(),
    })
}

fn parse_plan(content: &str) -> ParsedPlan {
    let mut phases = Vec::new();
    let mut phase_indexes = BTreeMap::new();
    let mut current_phase = None;
    let mut tasks = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let source_line = i32::try_from(index + 1).unwrap_or(i32::MAX);
        if let Some((ordinal, title)) = parse_phase_heading(line) {
            let phase_index = *phase_indexes.entry(ordinal).or_insert_with(|| {
                phases.push(ParsedPhase {
                    ordinal,
                    title: title.clone(),
                    source_line,
                    status: "pending".to_owned(),
                });
                phases.len() - 1
            });
            current_phase = Some(phase_index);
            continue;
        }
        let Some((checked, title)) = parse_checkbox(line) else {
            continue;
        };
        if title.is_empty() {
            continue;
        }
        tasks.push(ParsedTask {
            phase_ordinal: current_phase.map(|phase_index| phases[phase_index].ordinal),
            title,
            checked,
            source_line,
        });
    }
    for phase in &mut phases {
        let phase_tasks: Vec<_> = tasks
            .iter()
            .filter(|task| task.phase_ordinal == Some(phase.ordinal))
            .collect();
        let completed = phase_tasks.iter().filter(|task| task.checked).count();
        phase.status = if phase_tasks.is_empty() || completed == 0 {
            "pending".to_owned()
        } else if completed == phase_tasks.len() {
            "completed".to_owned()
        } else {
            "in_progress".to_owned()
        };
    }
    ParsedPlan { phases, tasks }
}

fn parse_phase_heading(line: &str) -> Option<(i32, String)> {
    let heading = line.trim_start().trim_start_matches('#').trim();
    let remainder = heading.strip_prefix("Phase ")?;
    let digit_count = remainder.chars().take_while(char::is_ascii_digit).count();
    if digit_count == 0 {
        return None;
    }
    let ordinal = remainder.get(..digit_count)?.parse().ok()?;
    let title = remainder
        .get(digit_count..)?
        .trim_start_matches([' ', '-', ':', '.', '—', '–'])
        .trim()
        .to_owned();
    Some((
        ordinal,
        if title.is_empty() {
            format!("Phase {ordinal}")
        } else {
            title
        },
    ))
}

fn parse_checkbox(line: &str) -> Option<(bool, String)> {
    let item = list_item_content(line.trim_start())?;
    let checkbox = item.strip_prefix('[')?;
    let check_mark = checkbox.chars().next()?;
    let checked = match check_mark {
        ' ' => false,
        'x' | 'X' => true,
        _ => return None,
    };
    let remainder = checkbox.strip_prefix(check_mark)?.strip_prefix(']')?;
    Some((checked, remainder.trim().to_owned()))
}

fn list_item_content(line: &str) -> Option<&str> {
    if let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
    {
        return Some(rest);
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let marker = line.get(digits..)?;
        return marker.strip_prefix(". ");
    }
    None
}

fn markdown_plan_title(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let heading = line.trim_start();
        if !heading.starts_with('#') {
            return None;
        }
        let title = heading.trim_start_matches('#').trim();
        parse_phase_heading(line)
            .is_none()
            .then(|| title.to_owned())
    })
}

/// Creates an independent task, optionally placing it in a current-plan phase.
///
/// # Errors
/// Returns an error when the project/phase does not exist or the database write fails.
pub async fn add_task(
    pool: &PgPool,
    project: &str,
    title: &str,
    description: Option<&str>,
    phase_ordinal: Option<i32>,
) -> Result<TaskResult, WorkflowError> {
    if title.trim().is_empty() {
        return Err(WorkflowError::EmptyField("task title"));
    }
    let project_id: Option<Uuid> = sqlx::query_scalar(query_sql!("workflow/add_task_01.sql"))
        .bind(project)
        .fetch_optional(pool)
        .await?;
    let Some(project_id) = project_id else {
        return Err(WorkflowError::NotFound("project"));
    };
    let phase_id = if let Some(ordinal) = phase_ordinal {
        let result: Option<Uuid> = sqlx::query_scalar(query_sql!("workflow/add_task_02.sql"))
            .bind(project_id)
            .bind(ordinal)
            .fetch_optional(pool)
            .await?;
        Some(result.ok_or(WorkflowError::NotFound("current plan phase"))?)
    } else {
        None
    };
    let task_id = Uuid::new_v4();
    sqlx::query(query_sql!("workflow/add_task_03.sql"))
        .bind(task_id)
        .bind(project_id)
        .bind(phase_id)
        .bind(title.trim())
        .bind(description)
        .execute(pool)
        .await?;
    if let Some(phase_id) = phase_id {
        refresh_phase_status(pool, phase_id).await?;
    }
    load_task(pool, task_id).await
}

/// Lists current-plan and standalone tasks in a project.
///
/// # Errors
/// Returns an error if the project query fails.
pub async fn task_list(pool: &PgPool, project: &str) -> Result<Vec<TaskResult>, WorkflowError> {
    Ok(sqlx::query_as(query_sql!("workflow/task_list_01.sql"))
        .bind(project)
        .fetch_all(pool)
        .await?)
}

/// Updates an agent-managed task status and refreshes its phase aggregate.
///
/// # Errors
/// Returns an error if the status is invalid, task is missing, or the database rejects the update.
pub async fn update_task_status(
    pool: &PgPool,
    task_id: Uuid,
    status: &str,
) -> Result<TaskResult, WorkflowError> {
    if !matches!(
        status,
        "pending" | "in_progress" | "blocked" | "done" | "cancelled"
    ) {
        return Err(WorkflowError::InvalidTaskStatus(status.to_owned()));
    }
    let phase_id: Option<Uuid> =
        sqlx::query_scalar(query_sql!("workflow/update_task_status_01.sql"))
            .bind(task_id)
            .bind(status)
            .fetch_optional(pool)
            .await?
            .ok_or(WorkflowError::NotFound("task"))?;
    if let Some(phase_id) = phase_id {
        refresh_phase_status(pool, phase_id).await?;
    }
    load_task(pool, task_id).await
}

/// Sets the focused task for a project without requiring a plan or session.
///
/// # Errors
/// Returns an error if the task does not belong to the project or the update fails.
pub async fn focus_task(pool: &PgPool, project: &str, task_id: Uuid) -> Result<(), WorkflowError> {
    let result = sqlx::query(query_sql!("workflow/focus_task_01.sql"))
        .bind(project)
        .bind(task_id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(WorkflowError::NotFound("task in project"));
    }
    Ok(())
}

async fn load_task(pool: &PgPool, task_id: Uuid) -> Result<TaskResult, WorkflowError> {
    sqlx::query_as(query_sql!("workflow/load_task_01.sql"))
        .bind(task_id)
        .fetch_optional(pool)
        .await?
        .ok_or(WorkflowError::NotFound("task"))
}

async fn refresh_phase_status(pool: &PgPool, phase_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(query_sql!("workflow/refresh_phase_status_01.sql"))
        .bind(phase_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Adds a commit, pull request, or implementation reference to a task.
///
/// # Errors
/// Returns an error if the task/repository is missing or the reference is invalid.
pub async fn add_task_reference(
    pool: &PgPool,
    task_id: Uuid,
    kind: &str,
    reference: &str,
    title: Option<&str>,
    url: Option<&str>,
    repository: Option<&str>,
) -> Result<TaskReferenceResult, WorkflowError> {
    if !matches!(kind, "commit" | "pull-request" | "implementation") {
        return Err(WorkflowError::InvalidReferenceKind(kind.to_owned()));
    }
    if reference.trim().is_empty() {
        return Err(WorkflowError::EmptyField("task reference"));
    }
    let project_id: Option<Uuid> =
        sqlx::query_scalar(query_sql!("workflow/add_task_reference_01.sql"))
            .bind(task_id)
            .fetch_optional(pool)
            .await?;
    let Some(project_id) = project_id else {
        return Err(WorkflowError::NotFound("task"));
    };
    let repository_id = if let Some(repository) = repository {
        let id: Option<Uuid> = sqlx::query_scalar(query_sql!("workflow/add_task_reference_02.sql"))
            .bind(project_id)
            .bind(repository)
            .fetch_optional(pool)
            .await?;
        Some(id.ok_or(WorkflowError::NotFound("repository"))?)
    } else {
        None
    };
    let id = Uuid::new_v4();
    sqlx::query(query_sql!("workflow/add_task_reference_03.sql"))
        .bind(id)
        .bind(task_id)
        .bind(repository_id)
        .bind(kind)
        .bind(reference.trim())
        .bind(title)
        .bind(url)
        .execute(pool)
        .await?;
    sqlx::query_as(query_sql!("workflow/add_task_reference_04.sql"))
        .bind(task_id)
        .bind(kind)
        .bind(reference.trim())
        .fetch_one(pool)
        .await
        .map_err(Into::into)
}

/// Records a validated decision, optionally linked to a current source and prior decision.
///
/// # Errors
/// Returns an error if project/source/superseded decision is missing or persistence fails.
pub async fn record_decision(
    pool: &PgPool,
    project: &str,
    title: &str,
    rationale: &str,
    source_path: Option<&str>,
    supersedes: Option<Uuid>,
) -> Result<String, WorkflowError> {
    record_decision_with_roots(
        pool,
        project,
        title,
        rationale,
        source_path,
        supersedes,
        None,
    )
    .await
}

/// Records a decision only against a source document beneath an allowed root.
///
/// # Errors
/// Returns an error if project/source/superseded decision is missing or persistence fails.
pub async fn record_decision_under_roots(
    pool: &PgPool,
    project: &str,
    title: &str,
    rationale: &str,
    source_path: Option<&str>,
    supersedes: Option<Uuid>,
    roots: &[String],
) -> Result<String, WorkflowError> {
    record_decision_with_roots(
        pool,
        project,
        title,
        rationale,
        source_path,
        supersedes,
        Some(roots),
    )
    .await
}

async fn record_decision_with_roots(
    pool: &PgPool,
    project: &str,
    title: &str,
    rationale: &str,
    source_path: Option<&str>,
    supersedes: Option<Uuid>,
    roots: Option<&[String]>,
) -> Result<String, WorkflowError> {
    if title.trim().is_empty() {
        return Err(WorkflowError::EmptyField("decision title"));
    }
    if rationale.trim().is_empty() {
        return Err(WorkflowError::EmptyField("decision rationale"));
    }
    let project_id: Option<Uuid> =
        sqlx::query_scalar(query_sql!("workflow/record_decision_with_roots_01.sql"))
            .bind(project)
            .fetch_optional(pool)
            .await?;
    let Some(project_id) = project_id else {
        return Err(WorkflowError::NotFound("project"));
    };
    let source = if let Some(path) = source_path {
        let value: Option<(Uuid, Uuid)> =
            sqlx::query_as(query_sql!("workflow/record_decision_with_roots_02.sql"))
                .bind(project_id)
                .bind(path)
                .bind(roots.map(<[String]>::to_vec))
                .fetch_optional(pool)
                .await?;
        Some(value.ok_or(WorkflowError::NotFound("current source document"))?)
    } else {
        None
    };
    let id = Uuid::new_v4();
    let mut transaction = pool.begin().await?;
    sqlx::query(query_sql!("workflow/record_decision_with_roots_03.sql"))
        .bind(id)
        .bind(project_id)
        .bind(source.map(|(document, _)| document))
        .bind(source.map(|(_, revision)| revision))
        .bind(title.trim())
        .bind(rationale.trim())
        .execute(&mut *transaction)
        .await?;
    if let Some(supersedes) = supersedes {
        let result = sqlx::query(query_sql!("workflow/record_decision_with_roots_04.sql"))
            .bind(id)
            .bind(supersedes)
            .bind(project_id)
            .execute(&mut *transaction)
            .await?;
        if result.rows_affected() == 0 {
            return Err(WorkflowError::NotFound("current decision to supersede"));
        }
    }
    transaction.commit().await?;
    Ok(id.to_string())
}

/// Records a checkpoint with the latest indexed repository and local-change snapshot.
///
/// # Errors
/// Returns an error if the project, optional task/session, or persistence operation fails.
pub async fn record_checkpoint(
    pool: &PgPool,
    input: CheckpointInput<'_>,
) -> Result<String, WorkflowError> {
    record_checkpoint_with_roots(pool, input, None).await
}

/// Records a checkpoint whose repository snapshot includes only allowed checkouts.
///
/// # Errors
/// Returns an error if the project, optional task/session, or persistence operation fails.
pub async fn record_checkpoint_under_roots(
    pool: &PgPool,
    input: CheckpointInput<'_>,
    roots: &[String],
) -> Result<String, WorkflowError> {
    record_checkpoint_with_roots(pool, input, Some(roots)).await
}

async fn record_checkpoint_with_roots(
    pool: &PgPool,
    input: CheckpointInput<'_>,
    roots: Option<&[String]>,
) -> Result<String, WorkflowError> {
    let CheckpointInput {
        project,
        title,
        summary,
        next_steps,
        blockers,
        task_id,
        session_id,
    } = input;
    if title.trim().is_empty() {
        return Err(WorkflowError::EmptyField("checkpoint title"));
    }
    if summary.trim().is_empty() {
        return Err(WorkflowError::EmptyField("checkpoint summary"));
    }
    let project_id: Option<Uuid> =
        sqlx::query_scalar(query_sql!("workflow/record_checkpoint_with_roots_01.sql"))
            .bind(project)
            .fetch_optional(pool)
            .await?;
    let Some(project_id) = project_id else {
        return Err(WorkflowError::NotFound("project"));
    };
    if let Some(task_id) = task_id {
        let task_exists: bool =
            sqlx::query_scalar(query_sql!("workflow/record_checkpoint_with_roots_02.sql"))
                .bind(task_id)
                .bind(project_id)
                .fetch_one(pool)
                .await?;
        if !task_exists {
            return Err(WorkflowError::NotFound("task in project"));
        }
    }
    if let Some(session_id) = session_id {
        let session_exists: bool =
            sqlx::query_scalar(query_sql!("workflow/record_checkpoint_with_roots_03.sql"))
                .bind(session_id)
                .bind(project_id)
                .fetch_one(pool)
                .await?;
        if !session_exists {
            return Err(WorkflowError::NotFound("session in project"));
        }
    }
    let (project_repositories, project_changes) = if let Some(roots) = roots {
        (
            repositories_under_roots(pool, Some(project), None, roots).await?,
            repository_changes_under_roots(pool, Some(project), roots).await?,
        )
    } else {
        (
            repositories(pool, Some(project), None).await?,
            repository_changes(pool, Some(project)).await?,
        )
    };
    let repository_state = json!({
        "repositories": project_repositories,
        "changes": project_changes
    });
    let id = Uuid::new_v4();
    sqlx::query(query_sql!("workflow/record_checkpoint_with_roots_04.sql"))
        .bind(id)
        .bind(project_id)
        .bind(session_id)
        .bind(task_id)
        .bind(title.trim())
        .bind(summary.trim())
        .bind(next_steps.trim())
        .bind(blockers.trim())
        .bind(repository_state)
        .execute(pool)
        .await?;
    Ok(id.to_string())
}

/// Starts an agent session independently of plans, tasks, or checkpoints.
///
/// # Errors
/// Returns an error if the project or optional task is missing.
pub async fn start_session(
    pool: &PgPool,
    project: &str,
    client_kind: &str,
    task_id: Option<Uuid>,
) -> Result<String, WorkflowError> {
    if client_kind.trim().is_empty() {
        return Err(WorkflowError::EmptyField("session client kind"));
    }
    let project_id: Option<Uuid> = sqlx::query_scalar(query_sql!("workflow/start_session_01.sql"))
        .bind(project)
        .fetch_optional(pool)
        .await?;
    let Some(project_id) = project_id else {
        return Err(WorkflowError::NotFound("project"));
    };
    if let Some(task_id) = task_id {
        let exists: bool = sqlx::query_scalar(query_sql!("workflow/start_session_02.sql"))
            .bind(task_id)
            .bind(project_id)
            .fetch_one(pool)
            .await?;
        if !exists {
            return Err(WorkflowError::NotFound("task in project"));
        }
    }
    let id = Uuid::new_v4();
    sqlx::query(query_sql!("workflow/start_session_03.sql"))
        .bind(id)
        .bind(project_id)
        .bind(task_id)
        .bind(client_kind.trim())
        .execute(pool)
        .await?;
    Ok(id.to_string())
}

/// Ends a previously recorded agent session.
///
/// # Errors
/// Returns an error if the session does not exist or database persistence fails.
pub async fn finish_session(
    pool: &PgPool,
    session_id: Uuid,
    summary: Option<&str>,
) -> Result<(), WorkflowError> {
    let result = sqlx::query(query_sql!("workflow/finish_session_01.sql"))
        .bind(session_id)
        .bind(summary)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(WorkflowError::NotFound("active session"));
    }
    Ok(())
}

/// Retrieves agent-ready project state without requiring session history.
///
/// # Errors
/// Returns an error if the project or any state query fails.
pub async fn current_workflow_state(
    pool: &PgPool,
    project: &str,
) -> Result<WorkflowState, WorkflowError> {
    let project_id: Option<Uuid> =
        sqlx::query_scalar(query_sql!("workflow/current_workflow_state_01.sql"))
            .bind(project)
            .fetch_optional(pool)
            .await?;
    let Some(project_id) = project_id else {
        return Err(WorkflowError::NotFound("project"));
    };
    let plan: Option<WorkflowPlan> =
        sqlx::query_as(query_sql!("workflow/current_workflow_state_02.sql"))
            .bind(project_id)
            .fetch_optional(pool)
            .await?;
    let plan = if plan.is_some() {
        plan
    } else {
        current_plan(pool, project)
            .await?
            .into_iter()
            .next()
            .map(|source| WorkflowPlan {
                id: None,
                title: source.title.unwrap_or_else(|| source.path.clone()),
                source_path: Some(source.path),
                status: "indexed".to_owned(),
                is_stale: source.freshness_status != "current",
                updated_at: Some(source.last_seen_at),
            })
    };
    let phases = if let Some(plan_id) =
        sqlx::query_scalar::<_, Uuid>(query_sql!("workflow/current_workflow_state_03.sql"))
            .bind(project_id)
            .fetch_optional(pool)
            .await?
    {
        sqlx::query_as(query_sql!("workflow/current_workflow_state_04.sql"))
            .bind(plan_id)
            .fetch_all(pool)
            .await?
    } else {
        Vec::new()
    };
    let tasks = task_list(pool, project).await?;
    let current_task_id: Option<Uuid> =
        sqlx::query_scalar(query_sql!("workflow/current_workflow_state_05.sql"))
            .bind(project_id)
            .fetch_one(pool)
            .await?;
    let current_task = if let Some(task_id) = current_task_id {
        Some(load_task(pool, task_id).await?)
    } else {
        None
    };
    let decisions = decisions(pool, project, 50)
        .await?
        .into_iter()
        .filter(|decision| decision.status == "validated")
        .collect();
    let checkpoints = checkpoints(pool, project, 5).await?;
    let current_changes = repository_changes(pool, Some(project)).await?;
    let changes_since_checkpoint = if let Some(checkpoint) = checkpoints.first() {
        let state: serde_json::Value = serde_json::from_str(&checkpoint.repository_state)
            .map_err(|error| WorkflowError::InvalidCheckpointState(error.to_string()))?;
        let saved_changes = state.get("changes").cloned().unwrap_or_else(|| json!([]));
        let saved_changes: Vec<RepositoryChange> = serde_json::from_value(saved_changes)
            .map_err(|error| WorkflowError::InvalidCheckpointState(error.to_string()))?;
        diff_repository_changes(current_changes, saved_changes)
    } else {
        current_changes
    };
    let task_references = sqlx::query_as(query_sql!("workflow/current_workflow_state_06.sql"))
        .bind(project_id)
        .fetch_all(pool)
        .await?;
    Ok(WorkflowState {
        project: project.to_owned(),
        plan,
        phases,
        tasks,
        current_task,
        decisions,
        checkpoints,
        changes_since_checkpoint,
        task_references,
    })
}

fn diff_repository_changes(
    current: Vec<RepositoryChange>,
    saved: Vec<RepositoryChange>,
) -> Vec<RepositoryChange> {
    let mut previous: HashMap<(String, String, String), RepositoryChange> = saved
        .into_iter()
        .map(|change| {
            (
                (
                    change.repository.clone(),
                    change.root_path.clone(),
                    change.path.clone(),
                ),
                change,
            )
        })
        .collect();
    let mut differences = Vec::new();
    for change in current {
        let key = (
            change.repository.clone(),
            change.root_path.clone(),
            change.path.clone(),
        );
        let differs = previous.get(&key).is_none_or(|old| {
            old.status_text != change.status_text
                || old.is_staged != change.is_staged
                || old.is_unstaged != change.is_unstaged
                || old.is_untracked != change.is_untracked
        });
        previous.remove(&key);
        if differs {
            differences.push(change);
        }
    }
    differences.extend(previous.into_values().map(|mut change| {
        "resolved".clone_into(&mut change.status_text);
        change.is_staged = false;
        change.is_unstaged = false;
        change.is_untracked = false;
        change
    }));
    differences.sort_by(|left, right| {
        (&left.repository, &left.root_path, &left.path).cmp(&(
            &right.repository,
            &right.root_path,
            &right.path,
        ))
    });
    differences
}

#[cfg(test)]
mod tests {
    use super::{
        diff_repository_changes, markdown_plan_title, parse_checkbox, parse_phase_heading,
        parse_plan,
    };
    use crate::RepositoryChange;

    #[test]
    fn parses_plan_checkboxes_without_assuming_phase_order() {
        let source = "# Project plan\n\n- [ ] Plan-only task\n\n## Phase 3 — Indexing\n- [x] Add extraction\n  * [ ] Cover errors\n\n## Phase 5: Workflow continuity\n1. [ ] Persist checkpoints\n";
        let parsed = parse_plan(source);
        assert_eq!(
            parsed
                .phases
                .iter()
                .map(|phase| (phase.ordinal, phase.title.as_str()))
                .collect::<Vec<_>>(),
            vec![(3, "Indexing"), (5, "Workflow continuity")]
        );
        assert_eq!(parsed.tasks.len(), 4);
        assert_eq!(parsed.tasks[0].phase_ordinal, None);
        assert_eq!(parsed.tasks[0].title, "Plan-only task");
        assert_eq!(parsed.tasks[1].phase_ordinal, Some(3));
        assert_eq!(parsed.tasks[1].title, "Add extraction");
        assert!(parsed.tasks[1].checked);
        assert_eq!(parsed.tasks[2].phase_ordinal, Some(3));
        assert_eq!(parsed.tasks[2].title, "Cover errors");
        assert_eq!(parsed.tasks[2].source_line, 7);
        assert_eq!(parsed.phases[0].status, "in_progress");
        assert_eq!(parsed.phases[1].status, "pending");
        assert_eq!(markdown_plan_title(source).as_deref(), Some("Project plan"));
    }

    #[test]
    fn rejects_non_checkbox_and_malformed_phase_lines() {
        assert_eq!(parse_phase_heading("## Phase X — Later"), None);
        assert_eq!(parse_checkbox("- [~] not checked"), None);
        assert_eq!(parse_checkbox("not a list item"), None);
    }

    #[test]
    fn reports_only_local_changes_different_from_the_latest_checkpoint() {
        let current = vec![
            change("src/kept.rs", "M", false, true, false),
            change("src/new.rs", "??", false, false, true),
        ];
        let saved = vec![
            change("src/kept.rs", "M", false, true, false),
            change("src/resolved.rs", "A ", true, false, false),
        ];
        let differences = diff_repository_changes(current, saved);
        assert_eq!(differences.len(), 2);
        assert_eq!(differences[0].path, "src/new.rs");
        assert_eq!(differences[1].path, "src/resolved.rs");
        assert_eq!(differences[1].status_text, "resolved");
    }

    fn change(
        path: &str,
        status_text: &str,
        is_staged: bool,
        is_unstaged: bool,
        is_untracked: bool,
    ) -> RepositoryChange {
        RepositoryChange {
            repository: "repo".to_owned(),
            root_path: "/repo".to_owned(),
            path: path.to_owned(),
            status_text: status_text.to_owned(),
            is_staged,
            is_unstaged,
            is_untracked,
        }
    }
}
