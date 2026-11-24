//! Deterministic local Git repository discovery and document indexing.

pub mod rust_review;

macro_rules! query_sql {
    ($path:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/sql/", $path))
    };
}

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::UNIX_EPOCH,
};

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use syn::visit::{self, Visit};
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};

/// Summary of a completed local indexing run.
#[derive(Debug, Default)]
pub struct IndexSummary {
    /// Repositories found.
    pub repositories: usize,
    /// Files whose metadata or content was recorded.
    pub indexed_files: usize,
    /// Persisted indexing-run identifiers.
    pub runs: usize,
    /// Paths that could not be inspected.
    pub failures: Vec<String>,
}

async fn replace_repository_changes(
    pool: &PgPool,
    location: Uuid,
    status_output: &str,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query(query_sql!("lib/replace_repository_changes_01.sql"))
        .bind(location)
        .execute(&mut *transaction)
        .await?;
    for line in status_output.lines() {
        let Some(status_text) = line.get(..2) else {
            continue;
        };
        let path = line.get(3..).unwrap_or_default();
        if path.is_empty() {
            continue;
        }
        let index_status = status_text.as_bytes().first().copied().unwrap_or(b' ');
        let worktree_status = status_text.as_bytes().get(1).copied().unwrap_or(b' ');
        sqlx::query(query_sql!("lib/replace_repository_changes_02.sql"))
            .bind(Uuid::new_v4())
            .bind(location)
            .bind(path)
            .bind(status_text)
            .bind(index_status != b' ' && status_text != "??")
            .bind(worktree_status != b' ' && status_text != "??")
            .bind(status_text == "??")
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// Refreshes Git repositories found under the supplied roots.
///
/// # Errors
///
/// Returns an error when the database cannot record or update an indexing run.
pub async fn refresh(pool: &PgPool, roots: &[PathBuf]) -> Result<IndexSummary, sqlx::Error> {
    index_roots(pool, roots, false).await
}

/// Reindexes every supported file beneath the supplied roots.
///
/// Unlike [`refresh`], this forces derived content and symbols to be rebuilt.
///
/// # Errors
///
/// Returns an error when the database cannot record or update an indexing run.
pub async fn rescan(pool: &PgPool, roots: &[PathBuf]) -> Result<IndexSummary, sqlx::Error> {
    index_roots(pool, roots, true).await
}

async fn index_roots(
    pool: &PgPool,
    roots: &[PathBuf],
    force: bool,
) -> Result<IndexSummary, sqlx::Error> {
    let mut summary = IndexSummary::default();
    for root in roots {
        let mut discovery_failures = Vec::new();
        let repositories = repositories_under(root, &mut discovery_failures);
        if !discovery_failures.is_empty() {
            let run = start_run(pool, root, None).await?;
            for failure in &discovery_failures {
                persist_failure(pool, run, None, "repository discovery", failure).await?;
            }
            finish_run(
                pool,
                run,
                RunCompletion {
                    location: None,
                    discovered: 0,
                    indexed: 0,
                    failed: discovery_failures.len(),
                    status: "partial",
                    error: None,
                },
            )
            .await?;
            summary.runs += 1;
            summary.failures.extend(discovery_failures);
        }

        for repository in repositories {
            let run = start_run(pool, &repository, None).await?;
            summary.runs += 1;
            match index_repository(pool, run, &repository, force).await {
                Ok((location, indexed, discovered)) => {
                    let failed_count: i64 =
                        sqlx::query_scalar(query_sql!("lib/index_roots_01.sql"))
                            .bind(run)
                            .fetch_one(pool)
                            .await?;
                    let status = if failed_count == 0 {
                        "completed"
                    } else {
                        "partial"
                    };
                    let persisted_failures: Vec<String> =
                        sqlx::query_scalar(query_sql!("lib/index_roots_02.sql"))
                            .bind(run)
                            .fetch_all(pool)
                            .await?;
                    summary.failures.extend(persisted_failures);
                    finish_run(
                        pool,
                        run,
                        RunCompletion {
                            location: Some(location),
                            discovered,
                            indexed,
                            failed: usize::try_from(failed_count).unwrap_or(usize::MAX),
                            status,
                            error: None,
                        },
                    )
                    .await?;
                    summary.repositories += 1;
                    summary.indexed_files += indexed;
                }
                Err(error) => {
                    let error_text = error.to_string();
                    persist_failure(
                        pool,
                        run,
                        Some(&repository),
                        "index repository",
                        &error_text,
                    )
                    .await?;
                    finish_run(
                        pool,
                        run,
                        RunCompletion {
                            location: None,
                            discovered: 0,
                            indexed: 0,
                            failed: 1,
                            status: "failed",
                            error: Some(&error_text),
                        },
                    )
                    .await?;
                    summary
                        .failures
                        .push(format!("{}: {error}", repository.display()));
                }
            }
        }
    }
    Ok(summary)
}

fn repositories_under(root: &Path, failures: &mut Vec<String>) -> BTreeSet<PathBuf> {
    let mut repositories = BTreeSet::new();
    if root.join(".git").exists() {
        repositories.insert(root.to_path_buf());
        return repositories;
    }
    let mut walker = WalkDir::new(root).into_iter();
    while let Some(entry) = walker.next() {
        match entry {
            Ok(entry) if entry.file_type().is_dir() && entry.path().join(".git").exists() => {
                repositories.insert(entry.path().to_path_buf());
                walker.skip_current_dir();
            }
            Ok(entry)
                if entry.file_type().is_dir()
                    && entry.file_name() != OsStr::new(".git")
                    && !keep_directory(&entry) =>
            {
                walker.skip_current_dir();
            }
            Ok(entry) if entry.file_name() == OsStr::new(".git") && entry.file_type().is_file() => {
                if let Some(parent) = entry.path().parent() {
                    repositories.insert(parent.to_path_buf());
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(error.to_string()),
        }
    }
    repositories
}

fn keep_directory(entry: &DirEntry) -> bool {
    !matches!(
        entry.file_name().to_str(),
        Some("target" | "node_modules" | ".cache" | "postgres" | ".venv" | "vendor" | ".git")
    )
}

async fn start_run(
    pool: &PgPool,
    root: &Path,
    location: Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(query_sql!("lib/start_run_01.sql"))
        .bind(Uuid::new_v4())
        .bind(location)
        .bind(root.to_string_lossy().as_ref())
        .bind(serde_json::json!({"scanner": "itc-memory-index"}))
        .fetch_one(pool)
        .await
}

struct RunCompletion<'a> {
    location: Option<Uuid>,
    discovered: usize,
    indexed: usize,
    failed: usize,
    status: &'a str,
    error: Option<&'a str>,
}

async fn finish_run(
    pool: &PgPool,
    run: Uuid,
    completion: RunCompletion<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(query_sql!("lib/finish_run_01.sql"))
        .bind(run)
        .bind(completion.location)
        .bind(completion.status)
        .bind(i32::try_from(completion.discovered).unwrap_or(i32::MAX))
        .bind(i32::try_from(completion.indexed).unwrap_or(i32::MAX))
        .bind(i32::try_from(completion.failed).unwrap_or(i32::MAX))
        .bind(completion.error)
        .execute(pool)
        .await?;
    Ok(())
}

async fn persist_failure(
    pool: &PgPool,
    run: Uuid,
    path: Option<&Path>,
    operation: &str,
    message: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(query_sql!("lib/persist_failure_01.sql"))
        .bind(Uuid::new_v4())
        .bind(run)
        .bind(path.map(|path| path.to_string_lossy().into_owned()))
        .bind(operation)
        .bind(message)
        .execute(pool)
        .await?;
    Ok(())
}

async fn index_repository(
    pool: &PgPool,
    run: Uuid,
    root: &Path,
    force: bool,
) -> Result<(Uuid, usize, usize), sqlx::Error> {
    let remote = git(root, &["remote", "get-url", "origin"])
        .unwrap_or_else(|| format!("local:{}", root.display()));
    let canonical_remote = canonical_remote(&remote);
    let name = root
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("repository");
    let project_id = upsert_project(pool, &slug(&canonical_remote), name).await?;
    let repository_id = upsert_repository(pool, project_id, &canonical_remote, name).await?;
    let git_state = observe_git_state(pool, run, repository_id, root).await?;
    let git_dir = git(root, &["rev-parse", "--git-dir"]).map_or_else(
        || root.join(".git"),
        |path| {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        },
    );
    let dirty = git_state.worktree.as_ref().map(|output| !output.is_empty());
    let location_id = upsert_location(
        pool,
        repository_id,
        root,
        &git_dir,
        git_state.branch.as_deref(),
        git_state.head.as_deref(),
        dirty,
    )
    .await?;
    sqlx::query(query_sql!("lib/index_repository_01.sql"))
        .bind(repository_id)
        .execute(pool)
        .await?;
    if let Some(changes) = git_state.worktree.as_deref() {
        replace_repository_changes(pool, location_id, changes).await?;
    }
    sqlx::query(query_sql!("lib/index_repository_02.sql"))
        .bind(run)
        .bind(location_id)
        .execute(pool)
        .await?;

    let (indexed, discovered) = index_repository_files(
        pool,
        RepositoryFiles {
            run,
            project: project_id,
            location: location_id,
            root,
            head: git_state.head.as_deref().filter(|head| !head.is_empty()),
            force,
        },
    )
    .await?;
    refresh_rust_dependencies(pool, run, root, location_id).await?;
    refresh_duplicate_status(pool, project_id).await?;
    let failed: bool = sqlx::query_scalar(query_sql!("lib/index_repository_03.sql"))
        .bind(run)
        .fetch_one(pool)
        .await?;
    if !failed {
        sqlx::query(query_sql!("lib/index_repository_04.sql"))
            .bind(location_id)
            .bind(run)
            .execute(pool)
            .await?;
    }
    Ok((location_id, indexed, discovered))
}

struct GitState {
    branch: Option<String>,
    head: Option<String>,
    worktree: Option<String>,
}

async fn observe_git_state(
    pool: &PgPool,
    run: Uuid,
    repository: Uuid,
    root: &Path,
) -> Result<GitState, sqlx::Error> {
    let branch = git(root, &["branch", "--show-current"]);
    let head = git(root, &["rev-parse", "HEAD"]).or_else(|| {
        branch
            .as_ref()
            .is_some_and(|branch| !branch.is_empty())
            .then(String::new)
    });
    let worktree = git(root, &["status", "--porcelain", "--untracked-files=all"]);
    for (operation, result) in [
        ("read Git branch", branch.as_ref()),
        ("read Git HEAD", head.as_ref()),
        ("read Git status", worktree.as_ref()),
    ] {
        if result.is_none() {
            persist_failure(pool, run, Some(root), operation, "Git command failed").await?;
        }
    }
    let previous: Option<(Option<String>, Option<String>)> =
        sqlx::query_as(query_sql!("lib/observe_git_state_01.sql"))
            .bind(hostname())
            .bind(root.to_string_lossy().as_ref())
            .fetch_optional(pool)
            .await?;
    let branch_changed = previous.as_ref().is_some_and(|(old, _)| {
        branch
            .as_ref()
            .is_some_and(|current| old.as_deref() != Some(current.as_str()))
    });
    let head_changed = previous.as_ref().is_some_and(|(_, old)| {
        head.as_ref()
            .is_some_and(|current| old.as_deref() != Some(current.as_str()))
    });
    sqlx::query(query_sql!("lib/observe_git_state_02.sql"))
        .bind(run)
        .bind(serde_json::json!({
            "branch_changed": branch_changed,
            "head_changed": head_changed,
            "current_branch": branch.as_deref(),
            "head_sha": head.as_deref().filter(|head| !head.is_empty()),
        }))
        .execute(pool)
        .await?;
    let subject = git(root, &["log", "-1", "--format=%s"]);
    if head.is_some() && subject.is_none() {
        persist_failure(
            pool,
            run,
            Some(root),
            "read Git commit subject",
            "Git command failed",
        )
        .await?;
    }
    record_repository_state(
        pool,
        repository,
        branch.as_deref(),
        head.as_deref().filter(|head| !head.is_empty()),
        subject.as_deref(),
    )
    .await?;
    Ok(GitState {
        branch,
        head,
        worktree,
    })
}

struct RepositoryFiles<'a> {
    run: Uuid,
    project: Uuid,
    location: Uuid,
    root: &'a Path,
    head: Option<&'a str>,
    force: bool,
}

const SHARED_CONFIG_CHECKOUTS: [(&str, &str); 4] = [
    (".cursor", "cursor_checkout"),
    (".codex", "codex_checkout"),
    (".claude", "claude_checkout"),
    (".copilot", "copilot_checkout"),
];

fn shared_config_checkout(root: &Path, path: &Path) -> Option<(&'static str, &'static str)> {
    SHARED_CONFIG_CHECKOUTS
        .iter()
        .copied()
        .find(|(directory, _)| path.starts_with(root.join(directory)))
}

async fn index_repository_files(
    pool: &PgPool,
    repository: RepositoryFiles<'_>,
) -> Result<(usize, usize), sqlx::Error> {
    let RepositoryFiles {
        run,
        project,
        location,
        root,
        head,
        force,
    } = repository;
    let mut indexed = 0;
    let mut discovered = 0;
    let mut checkout_heads = BTreeMap::new();
    let mut walker = WalkDir::new(root).into_iter();
    while let Some(entry) = walker.next() {
        match entry {
            Ok(entry)
                if entry.file_type().is_dir()
                    && shared_config_checkout(root, entry.path())
                        .is_some_and(|(directory, _)| entry.path() == root.join(directory)) =>
            {
                let (directory, key) =
                    shared_config_checkout(root, entry.path()).expect("matched shared checkout");
                match fs::symlink_metadata(entry.path().join(".git")) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        persist_failure(
                            pool,
                            run,
                            Some(entry.path()),
                            "inspect nested configuration Git marker",
                            &error.to_string(),
                        )
                        .await?;
                        walker.skip_current_dir();
                        continue;
                    }
                }
                let checkout_head =
                    observe_config_checkout(pool, run, entry.path(), directory, key).await?;
                if let Some(checkout_head) = checkout_head {
                    checkout_heads.insert(directory, checkout_head);
                } else {
                    walker.skip_current_dir();
                }
            }
            Ok(entry)
                if entry.file_type().is_dir()
                    && (!keep_directory(&entry)
                        || (entry.path() != root && entry.path().join(".git").exists())) =>
            {
                walker.skip_current_dir();
            }
            Ok(entry) if entry.file_type().is_file() => {
                let Some(kind) = classify(root, entry.path()) else {
                    continue;
                };
                discovered += 1;
                match index_file(
                    pool,
                    FileIndex {
                        run,
                        project,
                        location,
                        root,
                        path: entry.path(),
                        kind,
                        head: checkout_heads
                            .iter()
                            .find(|(directory, _)| entry.path().starts_with(root.join(directory)))
                            .map_or(head, |(_, checkout_head)| Some(checkout_head.as_str())),
                        force,
                    },
                )
                .await
                {
                    Ok(changed) => indexed += usize::from(changed),
                    Err(error) => {
                        persist_failure(
                            pool,
                            run,
                            Some(entry.path()),
                            "index file",
                            &error.to_string(),
                        )
                        .await?;
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                let path = error.path().unwrap_or(root);
                persist_failure(pool, run, Some(path), "walk repository", &error.to_string())
                    .await?;
            }
        }
    }
    Ok((indexed, discovered))
}

async fn observe_config_checkout(
    pool: &PgPool,
    run: Uuid,
    root: &Path,
    directory: &str,
    key: &str,
) -> Result<Option<String>, sqlx::Error> {
    let branch = git(root, &["branch", "--show-current"]);
    let head = git(root, &["rev-parse", "HEAD"]);
    let worktree = git(root, &["status", "--porcelain", "--untracked-files=all"]);
    let top_level = git(root, &["rev-parse", "--show-toplevel"]);
    let valid_root = top_level.as_ref().is_some_and(|top_level| {
        fs::canonicalize(top_level)
            .ok()
            .zip(fs::canonicalize(root).ok())
            .is_some_and(|(actual, expected)| actual == expected)
    });
    for (operation, result) in [
        ("read nested configuration Git branch", branch.as_ref()),
        ("read nested configuration Git HEAD", head.as_ref()),
        ("read nested configuration Git status", worktree.as_ref()),
    ] {
        if result.is_none() {
            persist_failure(pool, run, Some(root), operation, "Git command failed").await?;
        }
    }
    if !valid_root {
        persist_failure(
            pool,
            run,
            Some(root),
            "verify nested configuration Git root",
            "Git checkout root does not match the configuration directory",
        )
        .await?;
    }
    sqlx::query(query_sql!("lib/observe_git_state_02.sql"))
        .bind(run)
        .bind(serde_json::json!({
            key: {
                "path": directory,
                "verified": valid_root,
                "branch": branch.as_deref().filter(|branch| valid_root && !branch.is_empty()),
                "head_sha": head.as_deref().filter(|_| valid_root),
                "is_dirty": worktree.as_ref().filter(|_| valid_root).map(|status| !status.is_empty()),
            }
        }))
        .execute(pool)
        .await?;
    Ok(if branch.is_some() && worktree.is_some() && valid_root {
        head
    } else {
        None
    })
}

async fn record_repository_state(
    pool: &PgPool,
    repository: Uuid,
    branch: Option<&str>,
    head: Option<&str>,
    subject: Option<&str>,
) -> Result<(), sqlx::Error> {
    if let Some(branch) = branch.filter(|branch| !branch.is_empty()) {
        sqlx::query(query_sql!("lib/record_repository_state_01.sql"))
            .bind(Uuid::new_v4())
            .bind(repository)
            .bind(branch)
            .bind(head)
            .execute(pool)
            .await?;
    }
    if let Some(head) = head.filter(|head| !head.is_empty()) {
        sqlx::query(query_sql!("lib/record_repository_state_02.sql"))
            .bind(Uuid::new_v4())
            .bind(repository)
            .bind(head)
            .bind(subject)
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn upsert_project(pool: &PgPool, slug: &str, name: &str) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(query_sql!("lib/upsert_project_01.sql"))
        .bind(Uuid::new_v4())
        .bind(slug)
        .bind(name)
        .fetch_one(pool)
        .await
}

async fn upsert_repository(
    pool: &PgPool,
    project: Uuid,
    remote: &str,
    name: &str,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(query_sql!("lib/upsert_repository_01.sql"))
        .bind(Uuid::new_v4())
        .bind(project)
        .bind(remote)
        .bind(name)
        .fetch_one(pool)
        .await
}

async fn upsert_location(
    pool: &PgPool,
    repository: Uuid,
    root: &Path,
    git_dir: &Path,
    branch: Option<&str>,
    head: Option<&str>,
    dirty: Option<bool>,
) -> Result<Uuid, sqlx::Error> {
    let path = root.to_string_lossy();
    sqlx::query_scalar(query_sql!("lib/upsert_location_01.sql"))
        .bind(Uuid::new_v4())
        .bind(repository)
        .bind(hostname())
        .bind(path.as_ref())
        .bind(git_dir.to_string_lossy().as_ref())
        .bind(branch)
        .bind(head)
        .bind(dirty)
        .fetch_one(pool)
        .await
}

struct FileIndex<'a> {
    run: Uuid,
    project: Uuid,
    location: Uuid,
    root: &'a Path,
    path: &'a Path,
    kind: &'a str,
    head: Option<&'a str>,
    force: bool,
}

type ExistingDocumentState = Option<(Option<i64>, Option<i64>, Option<String>, bool, bool)>;

async fn index_file(pool: &PgPool, file: FileIndex<'_>) -> Result<bool, sqlx::Error> {
    let FileIndex {
        run,
        project,
        location,
        root,
        path,
        kind,
        head,
        force,
    } = file;
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            persist_failure(pool, run, Some(path), "read metadata", &error.to_string()).await?;
            return Ok(false);
        }
    };
    let size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
    let mtime = modified_time_ns(&metadata);
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    if metadata.len() > 2_000_000 {
        mark_document_seen_without_content(pool, location, &relative, size, mtime, kind).await?;
        return Ok(false);
    }
    let content = match fs::read(path) {
        Ok(value) if !value.contains(&0) => value,
        Ok(_) => {
            mark_document_seen_without_content(pool, location, &relative, size, mtime, kind)
                .await?;
            return Ok(false);
        }
        Err(error) => {
            persist_failure(pool, run, Some(path), "read content", &error.to_string()).await?;
            return Ok(false);
        }
    };
    let hash = content_hash(&content);
    let existing: ExistingDocumentState = sqlx::query_as(query_sql!("lib/index_file_01.sql"))
        .bind(location)
        .bind(&relative)
        .fetch_optional(pool)
        .await?;
    let has_required_chunks = existing
        .as_ref()
        .is_some_and(|(_, _, _, _, has_chunks)| *has_chunks);
    let has_required_symbols = path.extension().is_none_or(|extension| extension != "rs")
        || existing
            .as_ref()
            .is_some_and(|(_, _, _, indexed, _)| *indexed);
    if !force
        && existing
            .as_ref()
            .is_some_and(|(old_size, old_mtime, old_hash, _, _)| {
                *old_size == Some(size)
                    && *old_mtime == mtime
                    && old_hash.as_deref() == Some(hash.as_str())
            })
        && has_required_chunks
        && has_required_symbols
    {
        sqlx::query(query_sql!("lib/index_file_02.sql"))
            .bind(location)
            .bind(&relative)
            .bind(initial_freshness(kind, Path::new(&relative)))
            .execute(pool)
            .await?;
        if shared_config_checkout(root, path).is_some() {
            sqlx::query(query_sql!("lib/index_file_03.sql"))
                .bind(location)
                .bind(&relative)
                .bind(head)
                .execute(pool)
                .await?;
        }
        return Ok(false);
    }
    persist_indexed_document(
        pool,
        IndexedFileContent {
            run,
            project,
            location,
            path,
            relative: &relative,
            kind,
            head,
            size,
            mtime,
            hash: &hash,
            content: &content,
        },
    )
    .await?;
    Ok(true)
}

fn content_hash(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

fn modified_time_ns(metadata: &fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|time| i64::try_from(time.as_nanos()).unwrap_or(i64::MAX))
}

async fn mark_document_seen_without_content(
    pool: &PgPool,
    location: Uuid,
    relative: &str,
    size: i64,
    mtime: Option<i64>,
    kind: &str,
) -> Result<(), sqlx::Error> {
    let document: Option<Uuid> =
        sqlx::query_scalar(query_sql!("lib/mark_document_seen_without_content_01.sql"))
            .bind(location)
            .bind(relative)
            .bind(size)
            .bind(mtime)
            .bind(initial_freshness(kind, Path::new(relative)))
            .fetch_optional(pool)
            .await?;
    if let Some(document) = document {
        sqlx::query(query_sql!("lib/mark_document_seen_without_content_02.sql"))
            .bind(document)
            .execute(pool)
            .await?;
        sqlx::query(query_sql!("lib/mark_document_seen_without_content_03.sql"))
            .bind(document)
            .execute(pool)
            .await?;
    }
    Ok(())
}

struct IndexedFileContent<'a> {
    run: Uuid,
    project: Uuid,
    location: Uuid,
    path: &'a Path,
    relative: &'a str,
    kind: &'a str,
    head: Option<&'a str>,
    size: i64,
    mtime: Option<i64>,
    hash: &'a str,
    content: &'a [u8],
}

async fn persist_indexed_document(
    pool: &PgPool,
    file: IndexedFileContent<'_>,
) -> Result<(), sqlx::Error> {
    let IndexedFileContent {
        run,
        project,
        location,
        path,
        relative,
        kind,
        head,
        size,
        mtime,
        hash,
        content,
    } = file;
    let document: Uuid = sqlx::query_scalar(query_sql!("lib/persist_indexed_document_01.sql"))
        .bind(Uuid::new_v4())
        .bind(project)
        .bind(location)
        .bind(relative)
        .bind(kind)
        .bind(path.file_name().and_then(OsStr::to_str))
        .bind(size)
        .bind(mtime)
        .bind(initial_freshness(kind, Path::new(relative)))
        .fetch_one(pool)
        .await?;
    sqlx::query(query_sql!("lib/persist_indexed_document_02.sql"))
        .bind(document)
        .execute(pool)
        .await?;
    let revision: Uuid = sqlx::query_scalar(query_sql!("lib/persist_indexed_document_03.sql"))
        .bind(Uuid::new_v4())
        .bind(document)
        .bind(hash)
        .bind(head)
        .bind(size)
        .bind(mtime)
        .fetch_one(pool)
        .await?;
    if matches!(kind, "plan" | "documentation" | "handoff" | "source") {
        let text = String::from_utf8_lossy(content);
        sqlx::query(query_sql!("lib/persist_indexed_document_04.sql"))
            .bind(Uuid::new_v4())
            .bind(revision)
            .bind(text.as_ref())
            .execute(pool)
            .await?;
    } else {
        sqlx::query(query_sql!("lib/persist_indexed_document_05.sql"))
            .bind(revision)
            .execute(pool)
            .await?;
    }
    if path.extension().is_some_and(|extension| extension == "rs") {
        let source = String::from_utf8_lossy(content);
        sqlx::query(query_sql!("lib/persist_indexed_document_06.sql"))
            .bind(document)
            .execute(pool)
            .await?;
        match extract_rust_symbols(&source) {
            Ok(symbols) => {
                replace_symbols(pool, document, symbols).await?;
                sqlx::query(query_sql!("lib/persist_indexed_document_07.sql"))
                    .bind(document)
                    .execute(pool)
                    .await?;
            }
            Err(error) => {
                persist_failure(
                    pool,
                    run,
                    Some(path),
                    "extract Rust symbols",
                    &error.to_string(),
                )
                .await?;
                sqlx::query(query_sql!("lib/persist_indexed_document_08.sql"))
                    .bind(document)
                    .execute(pool)
                    .await?;
            }
        }
    }
    Ok(())
}

async fn replace_symbols(
    pool: &PgPool,
    document: Uuid,
    symbols: Vec<ExtractedSymbol>,
) -> Result<(), sqlx::Error> {
    sqlx::query(query_sql!("lib/replace_symbols_01.sql"))
        .bind(document)
        .execute(pool)
        .await?;
    for symbol in symbols {
        sqlx::query(query_sql!("lib/replace_symbols_02.sql"))
            .bind(Uuid::new_v4())
            .bind(document)
            .bind(&symbol.name)
            .bind(symbol.kind)
            .bind(&symbol.qualified_name)
            .bind(symbol.line)
            .bind(Option::<String>::None)
            .execute(pool)
            .await?;
    }
    Ok(())
}

fn find_cargo_manifest(root: &Path) -> Option<PathBuf> {
    let manifest = root.join("Cargo.toml");
    if manifest.is_file() {
        Some(manifest)
    } else {
        WalkDir::new(root)
            .into_iter()
            .filter_entry(|entry| {
                keep_directory(entry)
                    && (entry.path() == root || !entry.path().join(".git").exists())
            })
            .filter_map(Result::ok)
            .find(|entry| {
                entry.file_type().is_file() && entry.file_name() == OsStr::new("Cargo.toml")
            })
            .map(walkdir::DirEntry::into_path)
    }
}

async fn load_cargo_metadata(
    pool: &PgPool,
    run: Uuid,
    root: &Path,
    manifest: &Path,
) -> Result<Option<Value>, sqlx::Error> {
    let metadata = Command::new("cargo")
        .arg("metadata")
        .arg("--no-deps")
        .arg("--offline")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(manifest)
        .current_dir(root)
        .output();
    let output = match metadata {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            let message = String::from_utf8_lossy(&output.stderr);
            persist_failure(pool, run, Some(manifest), "cargo metadata", message.trim()).await?;
            return Ok(None);
        }
        Err(error) => {
            persist_failure(
                pool,
                run,
                Some(manifest),
                "cargo metadata",
                &error.to_string(),
            )
            .await?;
            return Ok(None);
        }
    };
    match serde_json::from_slice(&output.stdout) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) => {
            persist_failure(
                pool,
                run,
                Some(manifest),
                "parse cargo metadata",
                &error.to_string(),
            )
            .await?;
            Ok(None)
        }
    }
}

async fn refresh_rust_dependencies(
    pool: &PgPool,
    run: Uuid,
    root: &Path,
    location: Uuid,
) -> Result<(), sqlx::Error> {
    let manifest = find_cargo_manifest(root);
    let Some(manifest) = manifest else {
        sqlx::query(query_sql!("lib/refresh_rust_dependencies_01.sql"))
            .bind(location)
            .execute(pool)
            .await?;
        return Ok(());
    };
    let Some(metadata) = load_cargo_metadata(pool, run, root, &manifest).await? else {
        return Ok(());
    };
    let packages = metadata["packages"].as_array().into_iter().flatten();
    sqlx::query(query_sql!("lib/refresh_rust_dependencies_02.sql"))
        .bind(location)
        .execute(pool)
        .await?;
    for package in packages {
        let Some(manifest) = package["manifest_path"].as_str() else {
            continue;
        };
        let manifest = Path::new(manifest);
        if !manifest.starts_with(root) {
            continue;
        }
        let source_path = manifest
            .strip_prefix(root)
            .unwrap_or(manifest)
            .to_string_lossy()
            .to_string();
        let source_document: Option<Uuid> =
            sqlx::query_scalar(query_sql!("lib/refresh_rust_dependencies_03.sql"))
                .bind(location)
                .bind(source_path)
                .fetch_optional(pool)
                .await?;
        for dependency in package["dependencies"].as_array().into_iter().flatten() {
            let Some(name) = dependency["name"].as_str() else {
                continue;
            };
            let kind = dependency["kind"].as_str().unwrap_or("normal");
            let target = dependency["target"].as_str().unwrap_or("");
            sqlx::query(query_sql!("lib/refresh_rust_dependencies_04.sql"))
                .bind(Uuid::new_v4())
                .bind(location)
                .bind(source_document)
                .bind(name)
                .bind(dependency["req"].as_str().unwrap_or("*"))
                .bind(kind)
                .bind(target)
                .bind(dependency["optional"].as_bool().unwrap_or(false))
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

async fn refresh_duplicate_status(pool: &PgPool, project: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(query_sql!("lib/refresh_duplicate_status_01.sql"))
        .bind(project)
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("lib/refresh_duplicate_status_02.sql"))
        .bind(project)
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("lib/refresh_duplicate_status_03.sql"))
        .bind(project)
        .execute(pool)
        .await?;
    Ok(())
}

fn initial_freshness(kind: &str, path: &Path) -> &'static str {
    let path = path.to_string_lossy().to_lowercase();
    if path.contains("/superseded/")
        || path.starts_with("superseded/")
        || path.ends_with(".superseded")
    {
        "superseded"
    } else if path.contains("/archive/")
        || path.starts_with("archive/")
        || path.contains("/historical/")
        || path.starts_with("historical/")
        || path.contains("deprecated")
        || Path::new(&path)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("old"))
    {
        "historical"
    } else if path.contains("/tmp/")
        || path.starts_with("tmp/")
        || path.contains("/temp/")
        || path.starts_with("temp/")
        || (path.starts_with(".claude/") && !is_shared_client_document(&path))
        || (path.starts_with(".copilot/") && !is_shared_client_document(&path))
        || path.contains("/.claude/")
        || path.contains("/.copilot/")
    {
        "temporary"
    } else if kind == "superseded" {
        "superseded"
    } else {
        "current"
    }
}

fn classify(root: &Path, path: &Path) -> Option<&'static str> {
    let relative = path
        .strip_prefix(root)
        .ok()?
        .to_string_lossy()
        .to_lowercase();
    if [".codex/", ".claude/", ".copilot/"]
        .iter()
        .any(|prefix| relative.starts_with(prefix))
        && !is_shared_client_document(&relative)
    {
        return None;
    }
    if relative.contains("/plans/") || relative.starts_with("plans/") {
        Some("plan")
    } else if relative.contains("handoff") {
        Some("handoff")
    } else if path
        .extension()
        .is_some_and(|extension| extension == "md" || extension == "mdx" || extension == "txt")
        || (relative.starts_with(".cursor/rules/")
            && path.extension().is_some_and(|extension| extension == "mdc"))
    {
        Some("documentation")
    } else if matches!(
        path.extension().and_then(OsStr::to_str),
        Some("rs" | "toml" | "ts" | "tsx" | "js" | "py")
    ) {
        Some("source")
    } else {
        None
    }
}

fn is_shared_client_document(path: &str) -> bool {
    let markdown = Path::new(path)
        .extension()
        .is_some_and(|extension| extension == "md" || extension == "mdx");
    markdown
        && (matches!(
            path,
            ".codex/agents.md" | ".claude/claude.md" | ".copilot/copilot-instructions.md"
        ) || [
            ".codex/agents/",
            ".codex/skills/",
            ".codex/plans/",
            ".claude/agents/",
            ".claude/rules/",
            ".claude/skills/",
            ".claude/plans/",
            ".copilot/agents/",
            ".copilot/instructions/",
            ".copilot/skills/",
            ".copilot/plans/",
        ]
        .iter()
        .any(|prefix| path.starts_with(prefix)))
}

#[derive(Debug, PartialEq, Eq)]
struct ExtractedSymbol {
    name: String,
    kind: &'static str,
    qualified_name: String,
    line: i32,
}

struct SymbolVisitor<'a> {
    source: &'a str,
    scopes: Vec<String>,
    symbols: Vec<ExtractedSymbol>,
}

impl SymbolVisitor<'_> {
    fn add(&mut self, name: &str, kind: &'static str, span: proc_macro2::Span) {
        let mut components = self.scopes.clone();
        components.push(name.to_owned());
        let line = self
            .source
            .get(..span.byte_range().start.min(self.source.len()))
            .unwrap_or(self.source)
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        self.symbols.push(ExtractedSymbol {
            name: name.to_owned(),
            kind,
            qualified_name: components.join("::"),
            line: i32::try_from(line).unwrap_or(i32::MAX),
        });
    }
}

impl<'ast> Visit<'ast> for SymbolVisitor<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if let Some((_, items)) = &item.content {
            self.scopes.push(item.ident.to_string());
            for nested in items {
                self.visit_item(nested);
            }
            self.scopes.pop();
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let name = match item.self_ty.as_ref() {
            syn::Type::Path(path) => path
                .path
                .segments
                .last()
                .map_or_else(|| "impl".to_owned(), |segment| segment.ident.to_string()),
            _ => "impl".to_owned(),
        };
        self.scopes.push(name);
        visit::visit_item_impl(self, item);
        self.scopes.pop();
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.add(
            &item.sig.ident.to_string(),
            "function",
            item.sig.ident.span(),
        );
        visit::visit_item_fn(self, item);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        self.add(&item.ident.to_string(), "struct", item.ident.span());
        visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        self.add(&item.ident.to_string(), "enum", item.ident.span());
        visit::visit_item_enum(self, item);
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        self.add(&item.ident.to_string(), "trait", item.ident.span());
        self.scopes.push(item.ident.to_string());
        visit::visit_item_trait(self, item);
        self.scopes.pop();
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.add(&item.sig.ident.to_string(), "method", item.sig.ident.span());
        visit::visit_trait_item_fn(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        self.add(&item.ident.to_string(), "type", item.ident.span());
        visit::visit_item_type(self, item);
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        self.add(&item.ident.to_string(), "const", item.ident.span());
        visit::visit_item_const(self, item);
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        self.add(&item.ident.to_string(), "static", item.ident.span());
        visit::visit_item_static(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.add(&item.sig.ident.to_string(), "method", item.sig.ident.span());
        visit::visit_impl_item_fn(self, item);
    }
}

fn extract_rust_symbols(source: &str) -> Result<Vec<ExtractedSymbol>, syn::Error> {
    let file = syn::parse_file(source)?;
    let mut visitor = SymbolVisitor {
        source,
        scopes: Vec::new(),
        symbols: Vec::new(),
    };
    visitor.visit_file(&file);
    Ok(visitor.symbols)
}

fn git(root: &Path, arguments: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn canonical_remote(remote: &str) -> String {
    remote
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .replace("git@github.com:", "https://github.com/")
        .to_lowercase()
}

fn slug(value: &str) -> String {
    let mut previous_dash = false;
    value
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() {
                previous_dash = false;
                Some(character)
            } else if previous_dash {
                None
            } else {
                previous_dash = true;
                Some('-')
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(120)
        .collect()
}

fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_remote, classify, extract_rust_symbols, initial_freshness, refresh,
        repositories_under, rescan, slug,
    };
    use itc_memory_core::{DatabaseConfig, DatabaseConfigError};
    use sqlx::{
        Executor,
        postgres::{PgConnectOptions, PgPoolOptions},
    };
    use std::{collections::BTreeSet, fs, path::Path, path::PathBuf, process::Command};
    use uuid::Uuid;

    #[test]
    fn extracts_nested_rust_symbols_and_source_lines() {
        let source = "mod api {\n    pub struct Request;\n    impl Request {\n        pub fn send(&self) {}\n    }\n}\n";
        let symbols = extract_rust_symbols(source).expect("sample Rust parses");
        assert!(symbols.iter().any(|symbol| {
            symbol.qualified_name == "api::Request::send"
                && symbol.kind == "method"
                && symbol.line == 4
        }));
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol.qualified_name == "api::Request")
        );
        let trait_symbols = extract_rust_symbols("trait Transport { fn send(&self); }")
            .expect("sample trait parses");
        assert!(trait_symbols.iter().any(|symbol| {
            symbol.qualified_name == "Transport::send" && symbol.kind == "method"
        }));
    }

    #[test]
    fn recognizes_historical_and_temporary_documents() {
        assert_eq!(
            initial_freshness("documentation", Path::new("docs/archive/old.md")),
            "historical"
        );
        assert_eq!(
            initial_freshness("documentation", Path::new(".copilot/handoff.md")),
            "temporary"
        );
        assert_eq!(
            initial_freshness("documentation", Path::new("plans/superseded/plan.md")),
            "superseded"
        );
        assert_eq!(
            classify(Path::new("/repo"), Path::new("/repo/plans/roadmap.md")),
            Some("plan")
        );
        assert_eq!(
            classify(
                Path::new("/repo"),
                Path::new("/repo/.cursor/plans/roadmap.md")
            ),
            Some("plan")
        );
        assert_eq!(
            classify(
                Path::new("/repo"),
                Path::new("/repo/.cursor/rules/rust.mdc")
            ),
            Some("documentation")
        );
    }

    #[test]
    fn canonicalizes_remote_urls_for_multi_location_identity() {
        assert_eq!(
            canonical_remote("git@github.com:Owner/Project.git"),
            "https://github.com/owner/project"
        );
        assert_eq!(
            slug("https://github.com/owner/project"),
            "https-github-com-owner-project"
        );
    }

    #[test]
    fn an_explicit_repository_root_does_not_discover_nested_repositories() {
        let root = std::env::temp_dir().join(format!("itc-index-root-test-{}", Uuid::new_v4()));
        let _temporary_directory = TemporaryDirectory(root.clone());
        fs::create_dir_all(root.join(".git")).expect("create repository marker");
        fs::create_dir_all(root.join("nested/.git")).expect("create nested repository marker");

        let mut failures = Vec::new();
        let repositories = repositories_under(&root, &mut failures);

        assert_eq!(repositories, BTreeSet::from([root]));
        assert_eq!(failures, Vec::<String>::new());
    }

    #[tokio::test]
    async fn refresh_is_incremental_and_tracks_two_local_locations() {
        if std::path::Path::new(".env").is_file() {
            dotenvy::from_filename(".env").expect("load local test database configuration");
        }
        let config = match DatabaseConfig::from_environment() {
            Ok(config) => config,
            Err(DatabaseConfigError::Missing(_)) => {
                eprintln!("skipping database-backed indexing test: database is not configured");
                return;
            }
            Err(error) => panic!("invalid database configuration for indexing test: {error}"),
        };
        let admin_pool = itc_memory_db::connect(&config)
            .await
            .expect("connect to PostgreSQL");
        let schema = format!("itc_index_test_{}", Uuid::new_v4().simple());
        admin_pool
            .execute(format!(query_sql!("tests/create_test_schema.sql"), schema = schema).as_str())
            .await
            .expect("create isolated schema");
        let result = verify_multi_location_index(&config, &schema).await;
        admin_pool
            .execute(format!(query_sql!("tests/drop_test_schema.sql"), schema = schema).as_str())
            .await
            .expect("drop isolated schema");
        result.expect("verify incremental multi-location indexing");
    }

    #[tokio::test]
    async fn nested_cursor_checkout_is_indexed_as_product_context() {
        if std::path::Path::new(".env").is_file() {
            dotenvy::from_filename(".env").expect("load local test database configuration");
        }
        let config = match DatabaseConfig::from_environment() {
            Ok(config) => config,
            Err(DatabaseConfigError::Missing(_)) => {
                eprintln!("skipping database-backed indexing test: database is not configured");
                return;
            }
            Err(error) => panic!("invalid database configuration for indexing test: {error}"),
        };
        let admin_pool = itc_memory_db::connect(&config)
            .await
            .expect("connect to PostgreSQL");
        let schema = format!("itc_cursor_test_{}", Uuid::new_v4().simple());
        admin_pool
            .execute(format!(query_sql!("tests/create_test_schema.sql"), schema = schema).as_str())
            .await
            .expect("create isolated schema");
        let result = verify_cursor_checkout(&config, &schema).await;
        admin_pool
            .execute(format!(query_sql!("tests/drop_test_schema.sql"), schema = schema).as_str())
            .await
            .expect("drop isolated schema");
        result.expect("verify nested Cursor checkout indexing");
    }

    #[allow(clippy::too_many_lines)]
    async fn verify_cursor_checkout(
        config: &DatabaseConfig,
        schema: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let options = PgConnectOptions::new()
            .host(&config.host)
            .port(config.port)
            .username(&config.user)
            .password(&config.password)
            .database(&config.database)
            .options([("search_path", schema)]);
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        itc_memory_db::migrate(&pool).await?;

        let root = std::env::temp_dir().join(format!("itc-cursor-test-{}", Uuid::new_v4()));
        let _temporary_directory = TemporaryDirectory(root.clone());
        let product = root.join("product");
        let cursor = product.join(".cursor");
        let other = root.join("other");
        for repository in [&product, &other] {
            fs::create_dir_all(repository)?;
            run_git(repository, &["init", "--quiet"])?;
            run_git(
                repository,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/test/product.git",
                ],
            )?;
            fs::write(repository.join("README.md"), "# Product\n")?;
            run_git(repository, &["add", "README.md"])?;
            commit_fixture(repository, "product")?;
        }
        fs::create_dir_all(cursor.join("plans"))?;
        fs::create_dir_all(cursor.join("rules"))?;
        run_git(&cursor, &["init", "--quiet"])?;
        run_git(
            &cursor,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/test/itc-cursor.git",
            ],
        )?;
        fs::write(
            cursor.join("plans/roadmap.md"),
            "# Shared plan\n## Phase 1\n- [ ] cursor planning\n",
        )?;
        fs::write(cursor.join("rules/rust.mdc"), "cursor rule needle\n")?;
        run_git(&cursor, &["add", "."])?;
        commit_fixture(&cursor, "cursor fixture")?;
        run_git(&cursor, &["switch", "--quiet", "-c", "product-config"])?;
        fs::write(
            cursor.join(".git/internal.md"),
            "nested checkout secret marker\n",
        )?;
        fs::create_dir_all(cursor.join("rules/unrelated/.git"))?;
        fs::write(
            cursor.join("rules/unrelated/hidden.mdc"),
            "nested checkout secret marker\n",
        )?;
        fs::create_dir_all(product.join("unrelated/.git"))?;
        fs::write(
            product.join("unrelated/hidden.md"),
            "nested checkout secret marker\n",
        )?;
        fs::create_dir_all(other.join(".cursor/rules"))?;
        fs::write(
            other.join(".cursor/rules/local.mdc"),
            "ordinary cursor rule needle\n",
        )?;
        let client_configs = [
            (
                ".claude",
                "rules/shared.md",
                "claude context needle",
                "claude_checkout",
            ),
            (
                ".codex",
                "skills/shared/SKILL.md",
                "codex context needle",
                "codex_checkout",
            ),
            (
                ".copilot",
                "agents/shared.agent.md",
                "copilot context needle",
                "copilot_checkout",
            ),
        ];
        for (directory, file, content, _) in client_configs {
            let checkout = product.join(directory);
            let document = checkout.join(file);
            fs::create_dir_all(document.parent().ok_or("missing fixture parent")?)?;
            fs::write(&document, content)?;
            fs::write(checkout.join("config.toml"), "private-configuration-marker")?;
            run_git(&checkout, &["init", "--quiet"])?;
            run_git(&checkout, &["add", "."])?;
            commit_fixture(&checkout, "client fixture")?;
        }

        let product_root = product.to_string_lossy().into_owned();
        let other_root = other.to_string_lossy().into_owned();
        let project = "https-github-com-test-product";
        let roots = vec![product_root.clone()];
        let first = refresh(&pool, &[product.clone(), other.clone()]).await?;
        assert!(first.failures.is_empty(), "{:?}", first.failures);
        let nested_head =
            super::git(&cursor, &["rev-parse", "HEAD"]).ok_or("missing nested HEAD")?;
        let parent_head =
            super::git(&product, &["rev-parse", "HEAD"]).ok_or("missing parent HEAD")?;
        assert_ne!(nested_head, parent_head);
        let revision: (Option<String>, String) =
            sqlx::query_as(query_sql!("tests/cursor_document_01.sql"))
                .bind(&product_root)
                .bind(".cursor/plans/roadmap.md")
                .fetch_one(&pool)
                .await?;
        assert_eq!(revision, (Some(nested_head.clone()), "current".to_owned()));
        let runs = itc_memory_db::indexing_runs(&pool, 10).await?;
        let run = runs
            .iter()
            .find(|run| run.root_path == product_root)
            .ok_or("missing product run")?;
        assert_eq!(
            run.configuration["cursor_checkout"]["branch"],
            "product-config"
        );
        assert_eq!(
            run.configuration["cursor_checkout"]["head_sha"],
            nested_head
        );
        assert_eq!(run.configuration["cursor_checkout"]["verified"], true);
        for (directory, file, content, key) in client_configs {
            let checkout = product.join(directory);
            let checkout_head = super::git(&checkout, &["rev-parse", "HEAD"])
                .ok_or("missing client checkout HEAD")?;
            assert_eq!(run.configuration[key]["head_sha"], checkout_head);
            let path = format!("{directory}/{file}");
            let revision: (Option<String>, String) =
                sqlx::query_as(query_sql!("tests/cursor_document_01.sql"))
                    .bind(&product_root)
                    .bind(&path)
                    .fetch_one(&pool)
                    .await?;
            assert_eq!(revision, (Some(checkout_head), "current".to_owned()));
            let matches = itc_memory_db::search_file_lines_under_roots(
                &pool,
                content,
                Some(project),
                Some("current"),
                10,
                &roots,
            )
            .await?;
            assert_eq!(matches.len(), 1);
            assert_eq!(matches[0].path, path);
        }
        assert!(
            itc_memory_db::search_file_lines_under_roots(
                &pool,
                "private-configuration-marker",
                Some(project),
                None,
                10,
                &roots,
            )
            .await?
            .is_empty()
        );

        let rule = itc_memory_db::search_file_lines_under_roots(
            &pool,
            "cursor rule needle",
            Some(project),
            Some("current"),
            10,
            &roots,
        )
        .await?;
        assert_eq!(rule.len(), 1);
        assert_eq!(rule[0].path, ".cursor/rules/rust.mdc");
        assert_eq!(rule[0].root_path, product_root);
        let other_rule = itc_memory_db::search_file_lines_under_roots(
            &pool,
            "ordinary cursor rule needle",
            Some(project),
            Some("current"),
            10,
            std::slice::from_ref(&other_root),
        )
        .await?;
        assert_eq!(other_rule.len(), 1);
        assert!(
            itc_memory_db::search_file_lines_under_roots(
                &pool,
                "cursor rule needle",
                Some(project),
                Some("current"),
                10,
                std::slice::from_ref(&other_root),
            )
            .await?
            .iter()
            .all(|result| result.path != ".cursor/rules/rust.mdc")
        );
        assert!(
            itc_memory_db::search_file_lines_under_roots(
                &pool,
                "nested checkout secret marker",
                Some(project),
                None,
                10,
                &roots,
            )
            .await?
            .is_empty()
        );
        let context = itc_memory_db::context_pack_under_roots(
            &pool,
            project,
            "cursor planning",
            4_000,
            &roots,
        )
        .await?;
        assert!(
            context
                .sources
                .iter()
                .any(|source| source.path == ".cursor/plans/roadmap.md")
        );
        let plan = itc_memory_db::sync_plan(&pool, project, ".cursor/plans/roadmap.md").await?;
        assert_eq!(
            (plan.title.as_str(), plan.phases, plan.tasks),
            ("Shared plan", 1, 1)
        );

        run_git(&cursor, &["switch", "--quiet", "-c", "new-config"])?;
        run_git(
            &cursor,
            &[
                "-c",
                "user.name=Index Test",
                "-c",
                "user.email=index-test@example.invalid",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "new cursor head",
            ],
        )?;
        let updated_head =
            super::git(&cursor, &["rev-parse", "HEAD"]).ok_or("missing updated HEAD")?;
        let unchanged = refresh(&pool, std::slice::from_ref(&product)).await?;
        assert_eq!(unchanged.indexed_files, 0);
        let revision: (Option<String>, String) =
            sqlx::query_as(query_sql!("tests/cursor_document_01.sql"))
                .bind(&product_root)
                .bind(".cursor/plans/roadmap.md")
                .fetch_one(&pool)
                .await?;
        assert_eq!(revision.0.as_deref(), Some(updated_head.as_str()));
        let runs = itc_memory_db::indexing_runs(&pool, 1).await?;
        assert_eq!(
            runs[0].configuration["cursor_checkout"]["branch"],
            "new-config"
        );
        assert_eq!(
            runs[0].configuration["cursor_checkout"]["head_sha"],
            updated_head
        );

        fs::remove_file(cursor.join("plans/roadmap.md"))?;
        fs::write(
            cursor.join("rules/rust.mdc"),
            "changed cursor rule needle\n",
        )?;
        let changed = refresh(&pool, std::slice::from_ref(&product)).await?;
        assert!(changed.failures.is_empty(), "{:?}", changed.failures);
        assert_eq!(changed.indexed_files, 1);
        let revision: (Option<String>, String) =
            sqlx::query_as(query_sql!("tests/cursor_document_01.sql"))
                .bind(&product_root)
                .bind(".cursor/plans/roadmap.md")
                .fetch_one(&pool)
                .await?;
        assert_eq!(revision.1, "stale");
        let runs = itc_memory_db::indexing_runs(&pool, 1).await?;
        assert_eq!(runs[0].configuration["cursor_checkout"]["is_dirty"], true);
        assert!(
            itc_memory_db::sync_plan(&pool, project, ".cursor/plans/roadmap.md")
                .await
                .is_err()
        );

        fs::rename(cursor.join(".git/HEAD"), cursor.join(".git/HEAD.saved"))?;
        fs::remove_file(cursor.join("rules/rust.mdc"))?;
        let partial = refresh(&pool, std::slice::from_ref(&product)).await?;
        assert!(
            !partial.failures.is_empty(),
            "invalid nested checkout must be reported"
        );
        let revision: (Option<String>, String) =
            sqlx::query_as(query_sql!("tests/cursor_document_01.sql"))
                .bind(&product_root)
                .bind(".cursor/rules/rust.mdc")
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            revision.1, "current",
            "a partial scan must not mark unseen files stale"
        );
        fs::rename(cursor.join(".git/HEAD.saved"), cursor.join(".git/HEAD"))?;
        fs::remove_dir_all(&cursor)?;
        let removed = refresh(&pool, std::slice::from_ref(&product)).await?;
        assert!(removed.failures.is_empty(), "{:?}", removed.failures);
        let revision: (Option<String>, String) =
            sqlx::query_as(query_sql!("tests/cursor_document_01.sql"))
                .bind(&product_root)
                .bind(".cursor/rules/rust.mdc")
                .fetch_one(&pool)
                .await?;
        assert_eq!(revision.1, "stale");
        Ok(())
    }

    fn commit_fixture(repository: &Path, message: &str) -> Result<(), Box<dyn std::error::Error>> {
        run_git(
            repository,
            &[
                "-c",
                "user.name=Index Test",
                "-c",
                "user.email=index-test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        )
    }

    #[allow(clippy::too_many_lines)]
    async fn verify_multi_location_index(
        config: &DatabaseConfig,
        schema: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let options = PgConnectOptions::new()
            .host(&config.host)
            .port(config.port)
            .username(&config.user)
            .password(&config.password)
            .database(&config.database)
            .options([("search_path", schema)]);
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        itc_memory_db::migrate(&pool).await?;

        let root = std::env::temp_dir().join(format!("itc-index-test-{}", Uuid::new_v4()));
        let _temporary_directory = TemporaryDirectory(root.clone());
        let first = root.join("machine-a").join("sample");
        let second = root.join("machine-b").join("sample");
        for repository in [&first, &second] {
            fs::create_dir_all(repository.join("src"))?;
            fs::create_dir_all(repository.join("docs"))?;
            fs::write(
                repository.join("Cargo.toml"),
                "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nserde = \"1\"\n",
            )?;
            fs::write(
                repository.join("src/lib.rs"),
                "pub fn greet() -> &'static str { \"hello\" }\n",
            )?;
            fs::write(
                repository.join("docs/guide.md"),
                "# Guide\nLocal project guide.\n",
            )?;
            fs::write(
                repository.join("docs/duplicate.md"),
                "# Guide\nLocal project guide.\n",
            )?;
            run_git(repository, &["init", "--quiet"])?;
            run_git(
                repository,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/test/sample.git",
                ],
            )?;
            run_git(repository, &["add", "."])?;
            run_git(
                repository,
                &[
                    "-c",
                    "user.name=Index Test",
                    "-c",
                    "user.email=index-test@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "fixture",
                ],
            )?;
            fs::write(
                repository.join("src/local_change.rs"),
                "pub const LOCAL: bool = true;\n",
            )?;
        }
        let nested_repository = first.join("nested-repository");
        fs::create_dir_all(nested_repository.join(".git"))?;
        fs::create_dir_all(nested_repository.join("src"))?;
        fs::write(
            nested_repository.join("Cargo.toml"),
            "[package]\nname = \"nested\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        fs::write(
            nested_repository.join("src/lib.rs"),
            "pub fn unrelated() {}\n",
        )?;

        let initial = refresh(&pool, std::slice::from_ref(&root)).await?;
        if initial.repositories != 2 || initial.indexed_files < 6 {
            return Err(format!(
                "both repositories and their source files must be indexed; got {} repositories and {} files, failures: {:?}",
                initial.repositories, initial.indexed_files, initial.failures
            )
            .into());
        }
        let nested_documents: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_01.sql"))
            .fetch_one(&pool)
            .await?;
        if nested_documents != 0 {
            return Err("nested repositories must not be indexed through a parent checkout".into());
        }
        let duplicate_documents: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_02.sql"))
            .fetch_one(&pool)
            .await?;
        if duplicate_documents != 2 {
            let documents: String = sqlx::query_scalar(query_sql!("tests/unrelated_03.sql"))
                .fetch_one(&pool)
                .await?;
            return Err(format!(
                "expected two duplicate documents across distinct paths, got {duplicate_documents}; documents: {documents}"
            )
            .into());
        }
        run_git(&first, &["switch", "--quiet", "-c", "index-test-branch"])?;
        let again = refresh(&pool, std::slice::from_ref(&root)).await?;
        if again.repositories != 2 || again.indexed_files != 0 {
            return Err("unchanged repositories must not be re-indexed".into());
        }
        sqlx::query(query_sql!("tests/remove_source_chunk_01.sql"))
            .bind(first.to_string_lossy().as_ref())
            .execute(&pool)
            .await?;
        let backfill = refresh(&pool, std::slice::from_ref(&root)).await?;
        assert_eq!(
            backfill.indexed_files, 1,
            "missing source text is backfilled"
        );
        let lines = itc_memory_db::search_file_lines_under_roots(
            &pool,
            "pub fn greet()",
            None,
            Some("current"),
            10,
            &[first.to_string_lossy().into_owned()],
        )
        .await?;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].path, "src/lib.rs");
        assert_eq!(lines[0].line_number, Some(1));
        assert_eq!(
            lines[0].line.as_deref(),
            Some("pub fn greet() -> &'static str { \"hello\" }")
        );
        run_git(&first, &["add", "src/local_change.rs"])?;
        run_git(
            &first,
            &[
                "-c",
                "user.name=Index Test",
                "-c",
                "user.email=index-test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "record local change",
            ],
        )?;
        let committed = refresh(&pool, std::slice::from_ref(&root)).await?;
        if committed.indexed_files != 0 {
            return Err("a Git commit without content changes must not reindex files".into());
        }
        fs::write(
            first.join("docs/guide.md"),
            "# Guide\nUpdated local project guide.\n",
        )?;
        let changed = refresh(&pool, std::slice::from_ref(&root)).await?;
        if changed.indexed_files != 1 {
            return Err("a changed file must be re-indexed incrementally".into());
        }
        fs::remove_file(second.join("docs/guide.md"))?;
        refresh(&pool, std::slice::from_ref(&root)).await?;
        let stale_documents: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_04.sql"))
            .fetch_one(&pool)
            .await?;
        if stale_documents != 1 {
            return Err("a missing document must be retained as stale".into());
        }
        let forced = rescan(&pool, std::slice::from_ref(&root)).await?;
        if forced.repositories != 2 || forced.indexed_files < 6 {
            return Err("an explicit rescan must rebuild indexed documents".into());
        }
        let locations: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_05.sql"))
            .bind(first.to_string_lossy().as_ref())
            .bind(second.to_string_lossy().as_ref())
            .fetch_one(&pool)
            .await?;
        let repositories: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_06.sql"))
            .fetch_one(&pool)
            .await?;
        let symbols: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_07.sql"))
            .fetch_one(&pool)
            .await?;
        let dependencies: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_08.sql"))
            .fetch_one(&pool)
            .await?;
        let current_documents: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_09.sql"))
            .fetch_one(&pool)
            .await?;
        let failed_runs: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_10.sql"))
            .fetch_one(&pool)
            .await?;
        let local_changes: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_11.sql"))
            .fetch_one(&pool)
            .await?;
        let detected_state_changes: i64 = sqlx::query_scalar(query_sql!("tests/unrelated_12.sql"))
            .fetch_one(&pool)
            .await?;
        if (
            locations,
            repositories,
            symbols,
            dependencies,
            current_documents,
            failed_runs,
        ) != (2, 1, 2, 2, 1, 0)
            || local_changes < 2
            || detected_state_changes < 2
        {
            return Err("multi-location index, local changes, or run status mismatch".into());
        }
        fs::write(first.join("src/broken.rs"), "pub fn broken( {\n")?;
        let partial = refresh(&pool, std::slice::from_ref(&root)).await?;
        let recorded_failures: i64 = sqlx::query_scalar(query_sql!("tests/broken_01.sql"))
            .fetch_one(&pool)
            .await?;
        if partial.repositories != 2 || recorded_failures != 1 {
            return Err("a file extraction failure must persist without aborting the scan".into());
        }
        drop(pool);
        Ok(())
    }

    fn run_git(repository: &Path, arguments: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            )
            .into());
        }
        Ok(())
    }

    struct TemporaryDirectory(PathBuf);

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
