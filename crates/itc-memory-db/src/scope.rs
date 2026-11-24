//! Queries constrained to exact configured local checkout roots.

use std::{collections::BTreeSet, time::Instant};

use sqlx::PgPool;

use crate::{
    ContextPack, DecisionResult, FileLineMatch, ProjectSummary, RepositoryChange,
    RepositorySummary, SearchResult, SymbolResult, bound_context_sources, fallback_context_query,
    fuse_context_candidates,
};

/// Lists projects with counts restricted to exact allowed checkout locations.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn projects_under_roots(
    pool: &PgPool,
    roots: &[String],
) -> Result<Vec<ProjectSummary>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/projects_under_roots_01.sql"))
        .bind(roots)
        .fetch_all(pool)
        .await
}

/// Searches exact indexed lines and file paths under allowed checkout roots.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn search_file_lines_under_roots(
    pool: &PgPool,
    query: &str,
    project: Option<&str>,
    freshness: Option<&str>,
    limit: i64,
    roots: &[String],
) -> Result<Vec<FileLineMatch>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/search_file_lines_under_roots_01.sql"))
        .bind(query)
        .bind(project)
        .bind(freshness)
        .bind(limit)
        .bind(roots)
        .fetch_all(pool)
        .await
}

/// Lists repository locations at configured checkout roots.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn repositories_under_roots(
    pool: &PgPool,
    project: Option<&str>,
    dirty: Option<bool>,
    roots: &[String],
) -> Result<Vec<RepositorySummary>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/repositories_under_roots_01.sql"))
        .bind(project)
        .bind(dirty)
        .bind(roots)
        .fetch_all(pool)
        .await
}

/// Lists local changes only for repository locations at allowed checkout roots.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn repository_changes_under_roots(
    pool: &PgPool,
    project: Option<&str>,
    roots: &[String],
) -> Result<Vec<RepositoryChange>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/repository_changes_under_roots_01.sql"))
        .bind(project)
        .bind(roots)
        .fetch_all(pool)
        .await
}

/// Searches indexed text only within allowed checkout roots.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn search_under_roots(
    pool: &PgPool,
    query: &str,
    project: Option<&str>,
    freshness: Option<&str>,
    limit: i64,
    roots: &[String],
) -> Result<Vec<SearchResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/search_under_roots_01.sql"))
        .bind(query)
        .bind(project)
        .bind(freshness)
        .bind(limit)
        .bind(roots)
        .fetch_all(pool)
        .await
}

/// Searches Rust symbols only within allowed checkout roots.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn search_symbols_under_roots(
    pool: &PgPool,
    query: &str,
    project: Option<&str>,
    limit: i64,
    roots: &[String],
) -> Result<Vec<SymbolResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/search_symbols_under_roots_01.sql"))
        .bind(query)
        .bind(project)
        .bind(limit)
        .bind(roots)
        .fetch_all(pool)
        .await
}

/// Retrieves the current indexed plan only when its source is in an allowed root.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn current_plan_under_roots(
    pool: &PgPool,
    project: &str,
    roots: &[String],
) -> Result<Vec<SearchResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/current_plan_under_roots_01.sql"))
        .bind(project)
        .bind(roots)
        .fetch_all(pool)
        .await
}

/// Retrieves non-superseded decisions scoped to their source roots.
///
/// Project-level decisions without a source document are included; callers should first
/// verify that the project has at least one repository location within their allowed roots.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn decisions_under_roots(
    pool: &PgPool,
    project: &str,
    limit: i64,
    roots: &[String],
) -> Result<Vec<DecisionResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/decisions_under_roots_01.sql"))
        .bind(project)
        .bind(limit)
        .bind(roots)
        .fetch_all(pool)
        .await
}

/// Builds a bounded context pack from indexed documents at allowed checkout roots.
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn context_pack_under_roots(
    pool: &PgPool,
    project: &str,
    task: &str,
    max_context_bytes: usize,
    roots: &[String],
) -> Result<ContextPack, sqlx::Error> {
    build_context_pack(pool, project, task, max_context_bytes, Some(roots)).await
}

pub async fn context_pack_unrestricted(
    pool: &PgPool,
    project: &str,
    task: &str,
    max_context_bytes: usize,
) -> Result<ContextPack, sqlx::Error> {
    build_context_pack(pool, project, task, max_context_bytes, None).await
}

async fn build_context_pack(
    pool: &PgPool,
    project: &str,
    task: &str,
    max_context_bytes: usize,
    roots: Option<&[String]>,
) -> Result<ContextPack, sqlx::Error> {
    let retrieval_started = Instant::now();
    let exact = search_context_text(pool, project, task, roots).await?;
    let mut channels = vec![(exact, "exact-text", 1.5)];

    if let Some(expanded_query) = fallback_context_query(task) {
        let expanded = search_context_text(pool, project, &expanded_query, roots).await?;
        channels.push((expanded, "expanded-text", 1.0));

        let terms = expanded_query
            .split(" OR ")
            .take(6)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let patterns = terms
            .iter()
            .map(|term| format!("%{term}%"))
            .collect::<Vec<_>>();
        let symbols = search_symbols_for_terms(pool, project, &patterns, roots).await?;
        let dependencies = search_dependencies_for_terms(pool, project, &patterns, roots).await?;
        let search_query = terms.join(" OR ");

        let symbol_paths = symbols
            .iter()
            .map(|symbol| symbol.path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if !symbol_paths.is_empty() {
            let documents = search_documents_by_paths(
                pool,
                project,
                &symbol_paths,
                &search_query,
                roots,
                "symbol-source",
            )
            .await?;
            channels.push((documents, "symbol", 1.5));
        }

        let dependency_paths = dependencies
            .iter()
            .map(|dependency| dependency.path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if !dependency_paths.is_empty() {
            let documents = search_documents_by_paths(
                pool,
                project,
                &dependency_paths,
                &search_query,
                roots,
                "dependency-manifest",
            )
            .await?;
            channels.push((documents, "dependency", 1.5));
        }
    }

    let candidates = fuse_context_candidates(channels, 40);
    let (sources, mut metrics) = bound_context_sources(candidates, max_context_bytes);
    metrics.retrieval_latency_us =
        u64::try_from(retrieval_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    Ok(ContextPack {
        project: project.to_owned(),
        task: task.to_owned(),
        sources,
        metrics,
    })
}

async fn search_context_text(
    pool: &PgPool,
    project: &str,
    query: &str,
    roots: Option<&[String]>,
) -> Result<Vec<SearchResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/search_context_text_01.sql"))
        .bind(query)
        .bind(project)
        .bind(40_i64)
        .bind(roots.map(<[String]>::to_vec))
        .fetch_all(pool)
        .await
}

async fn search_symbols_for_terms(
    pool: &PgPool,
    project: &str,
    patterns: &[String],
    roots: Option<&[String]>,
) -> Result<Vec<SymbolResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/search_symbols_for_terms_01.sql"))
        .bind(project)
        .bind(patterns)
        .bind(roots.map(<[String]>::to_vec))
        .fetch_all(pool)
        .await
}

async fn search_dependencies_for_terms(
    pool: &PgPool,
    project: &str,
    patterns: &[String],
    roots: Option<&[String]>,
) -> Result<Vec<crate::DependencyResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/search_dependencies_for_terms_01.sql"))
        .bind(project)
        .bind(patterns)
        .bind(roots.map(<[String]>::to_vec))
        .fetch_all(pool)
        .await
}

async fn search_documents_by_paths(
    pool: &PgPool,
    project: &str,
    paths: &[String],
    query: &str,
    roots: Option<&[String]>,
    source_kind: &str,
) -> Result<Vec<SearchResult>, sqlx::Error> {
    sqlx::query_as(query_sql!("scope/search_documents_by_paths_01.sql"))
        .bind(project)
        .bind(paths)
        .bind(query)
        .bind(roots.map(<[String]>::to_vec))
        .bind(source_kind)
        .fetch_all(pool)
        .await
}
