use super::{
    CheckpointInput, ContextMetrics, MIGRATOR, ModelInferenceInput, SearchResult, add_task,
    add_task_reference, bound_context_sources, checkpoints, connect, connect_options, context_pack,
    context_pack_under_roots, current_plan, current_plan_under_roots, current_workflow_state,
    decisions, decisions_under_roots, evaluate_retrieval, finish_session, focus_task,
    fuse_context_candidates, indexing_runs, model_usage_summaries, projects_under_roots,
    record_checkpoint, record_checkpoint_under_roots, record_decision, record_decision_under_roots,
    record_model_inference, repositories, repositories_under_roots, repository_changes,
    repository_changes_under_roots, search, search_dependencies, search_dependencies_under_roots,
    search_file_lines_under_roots, search_symbols, search_symbols_under_roots, search_under_roots,
    start_session, sync_plan, task_list, update_task_status,
};
use itc_memory_core::DatabaseConfig;
use sqlx::{Executor, PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

#[test]
fn context_excerpt_budget_is_byte_bounded_and_utf8_safe() {
    let candidates = vec![
        SearchResult {
            project: "sample".to_owned(),
            root_path: None,
            path: "docs/first.md".to_owned(),
            document_kind: "documentation".to_owned(),
            title: None,
            excerpt: "éclair".to_owned(),
            last_seen_at: "now".to_owned(),
            freshness_status: "current".to_owned(),
            reason: "full-text match".to_owned(),
            score: 0.8,
        },
        SearchResult {
            project: "sample".to_owned(),
            root_path: None,
            path: "docs/second.md".to_owned(),
            document_kind: "documentation".to_owned(),
            title: None,
            excerpt: "later".to_owned(),
            last_seen_at: "now".to_owned(),
            freshness_status: "current".to_owned(),
            reason: "full-text match".to_owned(),
            score: 0.4,
        },
    ];
    let (sources, metrics) = bound_context_sources(candidates, 3);
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].excerpt, "éc");
    assert_eq!(
        metrics,
        ContextMetrics {
            source_count: 1,
            context_bytes: 3,
            average_score: 0.8,
            retrieval_latency_us: 0,
            max_context_bytes: 3,
        }
    );
}

#[test]
fn hybrid_fusion_rewards_consensus_and_deduplicates_paths() {
    let result = |path: &str| SearchResult {
        project: "sample".to_owned(),
        root_path: None,
        path: path.to_owned(),
        document_kind: "source".to_owned(),
        title: None,
        excerpt: path.to_owned(),
        last_seen_at: "now".to_owned(),
        freshness_status: "current".to_owned(),
        reason: "match".to_owned(),
        score: 0.0,
    };
    let fused = fuse_context_candidates(
        vec![
            (
                vec![
                    result("src/other.rs"),
                    result("src/db.rs"),
                    result("src/db.rs"),
                ],
                "exact-text",
                1.5,
            ),
            (
                vec![result("src/db.rs"), result("src/third.rs")],
                "symbol",
                1.5,
            ),
            (vec![result("src/db.rs")], "dependency", 1.5),
        ],
        10,
    );

    assert_eq!(fused.len(), 3);
    assert_eq!(fused[0].path, "src/db.rs");
    assert!(fused[0].reason.contains("channel=exact-text"));
    assert!(fused[0].reason.contains("channel=symbol"));
    assert!(fused[0].reason.contains("channel=dependency"));
    assert!(fused[0].score > fused[1].score);
}

#[tokio::test]
async fn migrations_create_the_initial_schema_in_an_isolated_schema() {
    let config =
        DatabaseConfig::from_environment().expect("database configuration for integration test");
    let admin_pool = connect(&config).await.expect("connect to PostgreSQL");
    let schema = format!("itc_memory_test_{}", Uuid::new_v4().simple());
    admin_pool
        .execute(format!(query_sql!("tests/create_test_schema.sql"), schema = schema).as_str())
        .await
        .expect("create isolated schema");

    let result = verify_schema(&config, &schema).await;
    admin_pool
        .execute(format!(query_sql!("tests/drop_test_schema.sql"), schema = schema).as_str())
        .await
        .expect("drop isolated schema");
    result.expect("verify migrated schema");
}

async fn verify_schema(config: &DatabaseConfig, schema: &str) -> Result<(), sqlx::Error> {
    let options = connect_options(config).options([("search_path", schema)]);
    let pool: PgPool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await.expect("apply migrations");
    let migration_count: i64 =
        sqlx::query_scalar(query_sql!("tests/verify_migration_count_01.sql"))
            .fetch_one(&pool)
            .await?;
    assert_eq!(migration_count, 2, "baseline plus one forward migration");
    let table_count: i64 = sqlx::query_scalar(query_sql!("tests/verify_schema_01.sql"))
        .fetch_one(&pool)
        .await?;
    // Twenty-three domain tables plus SQLx's migration ledger live in this schema.
    assert_eq!(table_count, 24);

    let project_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_schema_02.sql"))
        .bind(project_id)
        .bind("migration-test")
        .bind("Migration test")
        .execute(&pool)
        .await?;
    let slug: String = sqlx::query_scalar(query_sql!("tests/verify_schema_03.sql"))
        .bind(project_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(slug, "migration-test");
    verify_retrieval(&pool, project_id).await?;
    Ok(())
}

async fn insert_indexed_fixture_document(
    pool: &PgPool,
    project_id: Uuid,
    location_id: Uuid,
    path: &str,
    content: &str,
) -> Result<Uuid, sqlx::Error> {
    let document_id = Uuid::new_v4();
    let revision_id = Uuid::new_v4();
    let size = i64::try_from(content.len()).expect("fixture size fits in i64");
    sqlx::query(query_sql!("tests/insert_indexed_fixture_document_01.sql"))
        .bind(document_id)
        .bind(project_id)
        .bind(location_id)
        .bind(path)
        .bind(size)
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/insert_indexed_fixture_document_02.sql"))
        .bind(revision_id)
        .bind(document_id)
        .bind(Uuid::new_v4().to_string())
        .bind(size)
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/insert_indexed_fixture_document_03.sql"))
        .bind(Uuid::new_v4())
        .bind(revision_id)
        .bind(content)
        .execute(pool)
        .await?;
    Ok(document_id)
}

#[allow(clippy::too_many_lines)]
async fn verify_retrieval(pool: &PgPool, project_id: Uuid) -> Result<(), sqlx::Error> {
    let repository_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_retrieval_01.sql"))
        .bind(repository_id)
        .bind(project_id)
        .bind("https://github.com/test/migration-test")
        .bind("migration-test")
        .execute(pool)
        .await?;
    let location_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_retrieval_02.sql"))
        .bind(location_id)
        .bind(repository_id)
        .bind("test-machine")
        .bind("/test/migration-test")
        .bind("/test/migration-test/.git")
        .bind("main")
        .bind("test-head")
        .execute(pool)
        .await?;

    let filler = "Unrelated introductory material for the migration test. ".repeat(80);
    let document_content = format!(
        "# Migration test plan\n\n{filler}\n## Phase 1 — Architecture retrieval\n- [x] Chosen architecture supports reliable retrieval\n- [ ] Keep the workflow state recoverable\n"
    );
    let document_size = i64::try_from(document_content.len()).unwrap();
    let document_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_retrieval_03.sql"))
        .bind(document_id)
        .bind(project_id)
        .bind(location_id)
        .bind("plans/current.md")
        .bind("Current plan")
        .bind(document_size)
        .bind(1_i64)
        .execute(pool)
        .await?;
    let revision_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_retrieval_04.sql"))
        .bind(revision_id)
        .bind(document_id)
        .bind("test-content-hash")
        .bind("test-head")
        .bind(document_size)
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_retrieval_05.sql"))
        .bind(Uuid::new_v4())
        .bind(revision_id)
        .bind(&document_content)
        .execute(pool)
        .await?;
    let code_document_id = insert_indexed_fixture_document(
        pool,
        project_id,
        location_id,
        "src/db.rs",
        "Database connection code using connect_db and sqlx.",
    )
    .await?;
    let manifest_document_id = insert_indexed_fixture_document(
        pool,
        project_id,
        location_id,
        "Cargo.toml",
        "[dependencies]\nsqlx = \"0.8\"",
    )
    .await?;
    let retrieval_content = format!(
        "{}\n\nContext packs fuse exact and expanded retrieval channels and enforce the excerpt byte budget.\n\nquartz first evidence\n\nzircon second evidence",
        "Retrieval is mentioned in this unrelated introduction. ".repeat(120)
    );
    insert_indexed_fixture_document(
        pool,
        project_id,
        location_id,
        "docs/retrieval.md",
        &retrieval_content,
    )
    .await?;
    sqlx::query(query_sql!("tests/verify_retrieval_06.sql"))
        .bind(Uuid::new_v4())
        .bind(code_document_id)
        .bind("connect_db")
        .bind("function")
        .bind("Database::connect_db")
        .bind(12_i32)
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_retrieval_07.sql"))
        .bind(Uuid::new_v4())
        .bind(location_id)
        .bind(manifest_document_id)
        .bind("sqlx")
        .bind("^0.8")
        .bind("normal")
        .bind("")
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_retrieval_08.sql"))
        .bind(Uuid::new_v4())
        .bind(location_id)
        .bind("src/new.rs")
        .bind("??")
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_retrieval_09.sql"))
        .bind(Uuid::new_v4())
        .bind(project_id)
        .bind(document_id)
        .bind("Use deterministic search")
        .bind("Reproducible retrieval is auditable.")
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_retrieval_10.sql"))
        .bind(Uuid::new_v4())
        .bind(project_id)
        .bind("Index ready")
        .bind("The repository index is available.")
        .bind(serde_json::json!({"branch": "main"}))
        .execute(pool)
        .await?;
    let run_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_retrieval_11.sql"))
        .bind(run_id)
        .bind(location_id)
        .bind("/test/migration-test")
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_retrieval_12.sql"))
        .bind(Uuid::new_v4())
        .bind(run_id)
        .bind("src/unreadable.rs")
        .bind("read content")
        .bind("permission denied")
        .execute(pool)
        .await?;

    assert_eq!(
        repositories(pool, Some("migration-test"), Some(true))
            .await?
            .len(),
        1
    );
    assert_eq!(
        repository_changes(pool, Some("migration-test"))
            .await?
            .first()
            .map(|change| change.path.as_str()),
        Some("src/new.rs")
    );
    let matches = search(
        pool,
        "architecture",
        Some("migration-test"),
        Some("current"),
        12,
    )
    .await?;
    assert_eq!(
        matches.first().map(|result| result.path.as_str()),
        Some("plans/current.md")
    );
    assert!(matches[0].reason.contains("full-text"));
    let roots = vec!["/test/migration-test".to_owned()];
    let exact = search_file_lines_under_roots(
        pool,
        "## Phase 1 — Architecture retrieval",
        Some("migration-test"),
        Some("current"),
        10,
        &roots,
    )
    .await?;
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].path, "plans/current.md");
    assert_eq!(exact[0].line_number, Some(4));
    assert_eq!(
        exact[0].line.as_deref(),
        Some("## Phase 1 — Architecture retrieval")
    );
    assert!(
        search_file_lines_under_roots(
            pool,
            "## Phase 1 — Architecture retrieval",
            Some("migration-test"),
            None,
            10,
            &["/test/other-checkout".to_owned()],
        )
        .await?
        .is_empty()
    );
    assert!(
        search_file_lines_under_roots(
            pool,
            "## phase 1 — Architecture retrieval",
            Some("migration-test"),
            None,
            10,
            &roots,
        )
        .await?
        .is_empty()
    );
    assert!(
        search_file_lines_under_roots(pool, "%", Some("migration-test"), None, 10, &roots)
            .await?
            .is_empty()
    );
    assert_eq!(search_symbols(pool, "connect_db", None, 10).await?.len(), 1);
    assert_eq!(search_dependencies(pool, "sqlx", None, 10).await?.len(), 1);
    assert_eq!(current_plan(pool, "migration-test").await?.len(), 1);
    assert_eq!(decisions(pool, "migration-test", 10).await?.len(), 1);
    let checkpoint_results = checkpoints(pool, "migration-test", 10).await?;
    assert_eq!(checkpoint_results.len(), 1);
    assert!(checkpoint_results[0].repository_state.contains("branch"));
    let runs = indexing_runs(pool, 10).await?;
    assert_eq!(runs[0].failed_count, 1);
    assert!(
        runs[0]
            .error_summary
            .as_deref()
            .is_some_and(|message| message.contains("permission denied"))
    );
    record_model_inference(
        pool,
        &ModelInferenceInput {
            task_profile_id: Some("rust-reviewer".to_owned()),
            model_id: "qwen-rust".to_owned(),
            provider_id: "ollama-debby".to_owned(),
            provider_kind: "ollama".to_owned(),
            provider_model_id: "qwen3-coder:30b".to_owned(),
            hosted: false,
            trusted_lan: true,
            status: "succeeded".to_owned(),
            failure_code: None,
            fallback_used: false,
            latency_ms: 1250,
            input_tokens: Some(50),
            output_tokens: Some(12),
            estimated_cost_usd: None,
        },
    )
    .await?;
    record_model_inference(
        pool,
        &ModelInferenceInput {
            task_profile_id: Some("ask-explainer".to_owned()),
            model_id: "qwen-fast".to_owned(),
            provider_id: "ollama-local".to_owned(),
            provider_kind: "ollama".to_owned(),
            provider_model_id: "qwen3.5:0.8b".to_owned(),
            hosted: false,
            trusted_lan: false,
            status: "failed".to_owned(),
            failure_code: Some("empty_response".to_owned()),
            fallback_used: false,
            latency_ms: 1300,
            input_tokens: Some(48),
            output_tokens: None,
            estimated_cost_usd: None,
        },
    )
    .await?;
    let usage = model_usage_summaries(pool, 10).await?;
    let successful = usage
        .iter()
        .find(|item| item.model_id == "qwen-rust")
        .expect("successful model usage summary");
    assert_eq!(successful.call_count, 1);
    assert_eq!(successful.failure_count, 0);
    assert!(successful.trusted_lan);
    assert_eq!(successful.input_tokens, Some(50));
    assert_eq!(successful.output_tokens, Some(12));
    let failed = usage
        .iter()
        .find(|item| item.model_id == "qwen-fast")
        .expect("failed model usage summary");
    assert_eq!(failed.call_count, 1);
    assert_eq!(failed.failure_count, 1);
    let context = context_pack(pool, "migration-test", "architecture retrieval", 12).await?;
    assert_eq!(context.metrics.context_bytes, 12);
    assert_eq!(context.metrics.source_count, 1);
    let natural_language_context = context_pack(
        pool,
        "migration-test",
        "According to the source, what is Phase 1 for? Answer in one sentence.",
        2_000,
    )
    .await?;
    let plan_source = natural_language_context
        .sources
        .iter()
        .find(|source| source.path == "plans/current.md")
        .expect("retrieved source plan");
    assert!(plan_source.excerpt.contains("Architecture retrieval"));
    assert!(plan_source.excerpt.contains("<<Phase>>"));
    let passage_context = context_pack(
        pool,
        "migration-test",
        "Explain the retrieval channels used by context packs and how the excerpt byte budget is enforced.",
        16_000,
    )
    .await?;
    let passage = passage_context
        .sources
        .iter()
        .find(|source| source.path == "docs/retrieval.md")
        .expect("retrieved passage source");
    assert!(
        passage.excerpt.contains("fuse exact and expanded"),
        "expected the relevant passage, got: {}",
        passage.excerpt
    );
    let cross_paragraph = context_pack(pool, "migration-test", "quartz zircon", 16_000).await?;
    let passage = cross_paragraph
        .sources
        .iter()
        .find(|source| source.path == "docs/retrieval.md")
        .expect("retrieved cross-paragraph source");
    assert!(passage.excerpt.contains("<<quartz>>"));
    assert!(passage.excerpt.contains("<<zircon>>"));
    let plan_relevance =
        evaluate_retrieval(&natural_language_context.sources, &["plans/current.md"], 3);
    assert_eq!(plan_relevance.recall_at_k, 1.0);
    let task_query = "connect_db sqlx";
    let lexical_baseline = search(
        pool,
        task_query,
        Some("migration-test"),
        Some("current"),
        40,
    )
    .await?;
    let baseline_metrics = evaluate_retrieval(&lexical_baseline, &["src/db.rs", "Cargo.toml"], 2);
    assert_eq!(baseline_metrics.recall_at_k, 0.5);
    let hybrid_context = context_pack(pool, "migration-test", task_query, 4_000).await?;
    let retrieval = evaluate_retrieval(&hybrid_context.sources, &["src/db.rs", "Cargo.toml"], 2);
    assert_eq!(retrieval.recall_at_k, 1.0);
    assert_eq!(retrieval.precision_at_k, 1.0);
    assert_eq!(retrieval.reciprocal_rank, 1.0);
    assert!(hybrid_context.metrics.retrieval_latency_us > 0);
    verify_independent_workflows(pool).await?;
    verify_root_scoping(pool, project_id, repository_id).await?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn verify_root_scoping(
    pool: &PgPool,
    project_id: Uuid,
    repository_id: Uuid,
) -> Result<(), sqlx::Error> {
    let outside_location_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_root_scoping_01.sql"))
        .bind(outside_location_id)
        .bind(repository_id)
        .bind("test-machine")
        .bind("/test/migration-test/unrelated-checkout")
        .bind("/test/migration-test/unrelated-checkout/.git")
        .bind("outside")
        .bind("outside-head")
        .execute(pool)
        .await?;
    let outside_document_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_root_scoping_02.sql"))
        .bind(outside_document_id)
        .bind(project_id)
        .bind(outside_location_id)
        .bind("plans/private.md")
        .bind("Private plan")
        .bind(14_i64)
        .bind(1_i64)
        .execute(pool)
        .await?;
    let outside_revision_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_root_scoping_03.sql"))
        .bind(outside_revision_id)
        .bind(outside_document_id)
        .bind("outside-content-hash")
        .bind(14_i64)
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_root_scoping_04.sql"))
        .bind(Uuid::new_v4())
        .bind(outside_revision_id)
        .bind("private-only-marker")
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_root_scoping_05.sql"))
        .bind(Uuid::new_v4())
        .bind(outside_document_id)
        .bind("private_symbol")
        .bind("function")
        .bind("outside::private_symbol")
        .bind(1_i32)
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_root_scoping_06.sql"))
        .bind(Uuid::new_v4())
        .bind(outside_location_id)
        .bind(outside_document_id)
        .bind("private-crate")
        .bind("^1")
        .bind("normal")
        .bind("")
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_root_scoping_07.sql"))
        .bind(Uuid::new_v4())
        .bind(outside_location_id)
        .bind("private.rs")
        .bind("??")
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_root_scoping_08.sql"))
        .bind(Uuid::new_v4())
        .bind(project_id)
        .bind(outside_document_id)
        .bind("Outside-root decision")
        .bind("This source is not allowed.")
        .execute(pool)
        .await?;

    let allowed_location_id: Uuid =
        sqlx::query_scalar(query_sql!("tests/verify_root_scoping_09.sql"))
            .fetch_one(pool)
            .await?;
    let scoped_plan_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_root_scoping_10.sql"))
        .bind(scoped_plan_id)
        .bind(project_id)
        .bind(allowed_location_id)
        .bind("plans/scoped-current.md")
        .bind("Scoped current plan")
        .bind(20_i64)
        .bind(1_i64)
        .execute(pool)
        .await?;
    let scoped_plan_revision_id = Uuid::new_v4();
    sqlx::query(query_sql!("tests/verify_root_scoping_11.sql"))
        .bind(scoped_plan_revision_id)
        .bind(scoped_plan_id)
        .bind("scoped-plan-content-hash")
        .bind(20_i64)
        .execute(pool)
        .await?;
    sqlx::query(query_sql!("tests/verify_root_scoping_12.sql"))
        .bind(Uuid::new_v4())
        .bind(scoped_plan_revision_id)
        .bind("# Scoped current plan")
        .execute(pool)
        .await?;

    let roots = vec!["/test/migration-test".to_owned()];
    let projects = projects_under_roots(pool, &roots).await?;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].documents, 5);
    assert_eq!(
        repositories_under_roots(pool, None, None, &roots)
            .await?
            .len(),
        1
    );
    assert_eq!(
        repository_changes_under_roots(pool, None, &roots)
            .await?
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/after-checkpoint.rs", "src/new.rs"]
    );
    assert!(
        search_under_roots(pool, "private-only-marker", None, None, 10, &roots)
            .await?
            .is_empty()
    );
    assert!(
        search_symbols_under_roots(pool, "private_symbol", None, 10, &roots)
            .await?
            .is_empty()
    );
    assert!(
        search_dependencies_under_roots(pool, "private-crate", None, 10, &roots)
            .await?
            .is_empty()
    );
    let scoped_context =
        context_pack_under_roots(pool, "migration-test", "private_symbol sqlx", 4_000, &roots)
            .await?;
    assert!(
        scoped_context
            .sources
            .iter()
            .all(|source| source.root_path.as_deref() == Some("/test/migration-test"))
    );
    assert_eq!(
        current_plan_under_roots(pool, "migration-test", &roots)
            .await?
            .len(),
        1
    );
    let scoped_decisions = decisions_under_roots(pool, "migration-test", 10, &roots).await?;
    assert!(scoped_decisions.len() >= 3);
    assert!(
        scoped_decisions
            .iter()
            .all(|decision| decision.title != "Outside-root decision")
    );
    assert!(
        record_decision_under_roots(
            pool,
            "migration-test",
            "Reject out-of-root source",
            "The indexed source lies outside the allowed root.",
            Some("plans/private.md"),
            None,
            &roots,
        )
        .await
        .is_err()
    );
    record_checkpoint_under_roots(
        pool,
        CheckpointInput {
            project: "migration-test",
            title: "Scoped checkpoint",
            summary: "Its repository snapshot is root-limited.",
            next_steps: "",
            blockers: "",
            task_id: None,
            session_id: None,
        },
        &roots,
    )
    .await
    .expect("record a root-scoped checkpoint");
    let checkpoint = checkpoints(pool, "migration-test", 1).await?;
    assert!(
        checkpoint[0]
            .repository_state
            .contains("/test/migration-test")
    );
    assert!(
        !checkpoint[0]
            .repository_state
            .contains("/test/migration-test-evil")
    );
    Ok(())
}

async fn verify_independent_workflows(pool: &PgPool) -> Result<(), sqlx::Error> {
    let task = add_task(
        pool,
        "migration-test",
        "Standalone task before plan sync",
        None,
        None,
    )
    .await
    .expect("record independent task");
    let decision_id = record_decision(
        pool,
        "migration-test",
        "Keep workflow operations independent",
        "Agents should not need a prescribed Plan, Agent, Ask sequence.",
        None,
        None,
    )
    .await
    .expect("record decision without a plan");
    let initial_checkpoint = record_checkpoint(
        pool,
        CheckpointInput {
            project: "migration-test",
            title: "Independent checkpoint",
            summary: "Checkpoint can be recorded before a session or plan sync.",
            next_steps: "",
            blockers: "",
            task_id: None,
            session_id: None,
        },
    )
    .await
    .expect("record checkpoint without a task or session");
    assert_ne!(decision_id, "");
    assert_ne!(initial_checkpoint, "");

    verify_plan_driven_workflow(pool, &task.id).await
}

async fn verify_plan_driven_workflow(
    pool: &PgPool,
    standalone_task_id: &str,
) -> Result<(), sqlx::Error> {
    let plan = sync_plan(pool, "migration-test", "plans/current.md")
        .await
        .expect("sync plan checkboxes");
    assert_eq!(plan.phases, 1);
    assert_eq!(plan.tasks, 2);
    let tasks = task_list(pool, "migration-test")
        .await
        .expect("list imported and standalone tasks");
    assert_eq!(tasks.len(), 3);
    assert!(
        tasks
            .iter()
            .any(|candidate| candidate.id == standalone_task_id)
    );
    let checkbox_task = tasks
        .iter()
        .find(|candidate| candidate.checkbox_checked)
        .expect("checked checkbox task");
    let checkbox_task_id_text = checkbox_task.id.clone();
    let checkbox_task_id = Uuid::parse_str(&checkbox_task_id_text).expect("task UUID");
    let pending = tasks
        .iter()
        .find(|candidate| candidate.status == "pending" && candidate.id != standalone_task_id)
        .expect("pending checkbox task");
    let pending_id = Uuid::parse_str(&pending.id).expect("task UUID");
    update_task_status(pool, pending_id, "in_progress")
        .await
        .expect("update task status");
    focus_task(pool, "migration-test", checkbox_task_id)
        .await
        .expect("focus task");
    add_task_reference(
        pool,
        checkbox_task_id,
        "commit",
        "abc123",
        Some("Implement retrieval"),
        None,
        Some("migration-test"),
    )
    .await
    .expect("attach commit reference");
    let session_id = Uuid::parse_str(
        &start_session(pool, "migration-test", "copilot", Some(checkbox_task_id))
            .await
            .expect("start agent session"),
    )
    .expect("session UUID");
    record_checkpoint(
        pool,
        CheckpointInput {
            project: "migration-test",
            title: "Agent handoff",
            summary: "Retrieval work is indexed and validated.",
            next_steps: "Continue the remaining workflow task.",
            blockers: "PostgreSQL integration is still required.",
            task_id: Some(checkbox_task_id),
            session_id: Some(session_id),
        },
    )
    .await
    .expect("record handoff checkpoint");
    verify_workflow_staleness_and_state(pool, &checkbox_task_id_text, session_id).await
}

async fn verify_workflow_staleness_and_state(
    pool: &PgPool,
    current_task_id: &str,
    session_id: Uuid,
) -> Result<(), sqlx::Error> {
    record_decision(
        pool,
        "migration-test",
        "Keep retrieval source-backed",
        "The indexed plan constrains workflow retrieval.",
        Some("plans/current.md"),
        None,
    )
    .await
    .expect("record source-validated decision");
    finish_session(pool, session_id, Some("Saved workflow handoff"))
        .await
        .expect("finish agent session");
    let revision_id = Uuid::new_v4();
    sqlx::query(query_sql!(
        "tests/verify_workflow_staleness_and_state_01.sql"
    ))
    .execute(pool)
    .await?;
    sqlx::query(query_sql!(
        "tests/verify_workflow_staleness_and_state_02.sql"
    ))
    .bind(revision_id)
    .execute(pool)
    .await?;
    sqlx::query(query_sql!(
        "tests/verify_workflow_staleness_and_state_03.sql"
    ))
    .bind(Uuid::new_v4())
    .execute(pool)
    .await?;

    let tasks = task_list(pool, "migration-test")
        .await
        .expect("reload task states");
    let state = current_workflow_state(pool, "migration-test")
        .await
        .expect("retrieve agent recovery state");
    assert_eq!(state.plan.as_ref().map(|plan| plan.is_stale), Some(true));
    assert_eq!(
        state
            .current_task
            .as_ref()
            .map(|current| current.id.as_str()),
        Some(current_task_id)
    );
    assert!(
        tasks
            .iter()
            .any(|candidate| candidate.status == "in_progress")
    );
    assert_eq!(state.task_references.len(), 1);
    assert_eq!(state.decisions.len(), 3);
    assert!(state.decisions.iter().any(|decision| decision.is_stale));
    assert_eq!(state.checkpoints.len(), 3);
    assert_eq!(state.changes_since_checkpoint.len(), 1);
    Ok(())
}
