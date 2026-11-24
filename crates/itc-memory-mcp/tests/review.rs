//! Exercises the Rust review pipeline through MCP with PostgreSQL and a local mock model.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, bail};
use axum::{Json, Router, extract::State, routing::post};
use itc_memory_core::DatabaseConfig;
use itc_memory_db::{connect, migrate, projects_under_roots};
use itc_memory_mcp::ToolServer;
use itc_memory_model::{ModelRegistry, RegistryConfig};
use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde_json::{Value, json};
use sqlx::{
    Executor,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::{net::TcpListener, sync::Mutex};
use uuid::Uuid;

macro_rules! query_sql {
    ($path:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/sql/", $path))
    };
}

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove review fixture");
    }
}

async fn mock_chat(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    requests.lock().await.push(request);
    Json(json!({"message": {"content": "Review result: src/lib.rs:1"}}))
}

#[tokio::test]
async fn review_runs_clippy_and_supplies_source_and_diagnostics_to_the_model() -> anyhow::Result<()>
{
    let config = DatabaseConfig::from_environment()?;
    let admin = connect(&config).await?;
    let schema = format!("itc_review_test_{}", Uuid::new_v4().simple());
    admin
        .execute(
            format!(
                query_sql!("tests/create_review_schema.sql"),
                schema = schema
            )
            .as_str(),
        )
        .await?;
    let result = run_review_test(&config, &schema).await;
    admin
        .execute(format!(query_sql!("tests/drop_review_schema.sql"), schema = schema).as_str())
        .await?;
    result
}

#[allow(clippy::too_many_lines)]
async fn run_review_test(config: &DatabaseConfig, schema: &str) -> anyhow::Result<()> {
    let options = PgConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.user)
        .password(&config.password)
        .database(&config.database)
        .options([("search_path", schema)]);
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await?;
    migrate(&pool).await?;

    let root = std::env::temp_dir().join(format!("itc-mcp-review-{}", Uuid::new_v4()));
    let _fixture = Fixture(root.clone());
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"review-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn answer() -> u32 { \"incorrect\" }\n",
    )?;
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Review Test",
            "-c",
            "user.email=review-test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "review fixture\n\nCo-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .status()?;
        if !status.success() {
            bail!("could not initialize review fixture checkout");
        }
    }
    let cursor = root.join(".cursor");
    std::fs::create_dir_all(cursor.join("rules"))?;
    std::fs::write(cursor.join("rules/review.mdc"), "shared review rule\n")?;
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Review Test",
            "-c",
            "user.email=review-test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "cursor fixture",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&cursor)
            .status()?;
        if !status.success() {
            bail!("could not initialize nested Cursor fixture checkout");
        }
    }
    let summary = itc_memory_index::refresh(&pool, std::slice::from_ref(&root)).await?;
    assert!(
        summary.failures.is_empty(),
        "indexing failed: {:?}",
        summary.failures
    );
    let root_string = root.to_string_lossy().into_owned();
    let roots = vec![root_string.clone()];
    let projects = projects_under_roots(&pool, &roots).await?;
    let project = projects
        .first()
        .context("missing indexed review fixture")?
        .slug
        .clone();

    let requests = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/api/chat", post(mock_chat))
        .with_state(requests.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let model_task = tokio::spawn(async move { axum::serve(listener, app).await });
    let mut registry: RegistryConfig = serde_json::from_str(include_str!("fixtures/models.json"))?;
    registry.providers[0].base_url = format!("http://{address}");
    registry.providers[0].trusted_lan = false;
    let server = ToolServer::new(
        pool.clone(),
        roots,
        false,
        false,
        ModelRegistry::new(registry)?,
    );
    let (client_transport, server_transport) = tokio::io::duplex(262_144);
    let server_task = tokio::spawn(async move {
        server.serve(server_transport).await?.waiting().await?;
        Ok::<(), anyhow::Error>(())
    });
    let client = ().serve(client_transport).await?;

    let search = client
        .call_tool(
            CallToolRequestParams::new("search_files").with_arguments(
                json!({"query": "pub fn answer()", "project": project})
                    .as_object()
                    .context("search arguments")?
                    .clone(),
            ),
        )
        .await?;
    let text = search
        .content
        .first()
        .and_then(|block| block.as_text())
        .context("missing search result")?;
    let lines: Value = serde_json::from_str(&text.text)?;
    assert_eq!(lines[0]["path"], "src/lib.rs");
    assert_eq!(lines[0]["line_number"], 1);
    assert_eq!(lines[0]["line"], "pub fn answer() -> u32 { \"incorrect\" }");

    let shared_rule = client
        .call_tool(
            CallToolRequestParams::new("search_files").with_arguments(
                json!({"query": "shared review rule", "project": project})
                    .as_object()
                    .context("shared rule search arguments")?
                    .clone(),
            ),
        )
        .await?;
    let text = shared_rule
        .content
        .first()
        .and_then(|block| block.as_text())
        .context("missing shared rule search result")?;
    let lines: Value = serde_json::from_str(&text.text)?;
    assert_eq!(lines[0]["project"], project);
    assert_eq!(lines[0]["root_path"], root_string);
    assert_eq!(lines[0]["path"], ".cursor/rules/review.mdc");
    assert_eq!(lines[0]["line"], "shared review rule");

    let denied = client
        .call_tool(
            CallToolRequestParams::new("lint_rust").with_arguments(
                json!({"project": project, "root_path": "/tmp/not-allowed"})
                    .as_object()
                    .context("lint arguments")?
                    .clone(),
            ),
        )
        .await;
    assert!(
        denied.is_err() || denied.is_ok_and(|result| result.is_error == Some(true)),
        "lint must reject roots outside the allowlist"
    );

    let lint = client
        .call_tool(
            CallToolRequestParams::new("lint_rust").with_arguments(
                json!({"project": project, "root_path": root_string})
                    .as_object()
                    .context("lint arguments")?
                    .clone(),
            ),
        )
        .await?;
    let text = lint
        .content
        .first()
        .and_then(|block| block.as_text())
        .context("missing lint result")?;
    let lint_result: Value = serde_json::from_str(&text.text)?;
    assert_eq!(lint_result["success"], false);
    assert!(
        lint_result["diagnostics"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );

    let arguments = json!({"project": project, "root_path": root_string, "scope": "files", "files": ["src/lib.rs"]});
    let result = client
        .call_tool(
            CallToolRequestParams::new("review_rust")
                .with_arguments(arguments.as_object().context("review arguments")?.clone()),
        )
        .await?;
    assert_ne!(result.is_error, Some(true), "review failed: {result:?}");
    let text = result
        .content
        .first()
        .and_then(|block| block.as_text())
        .context("missing review result")?;
    let review: Value = serde_json::from_str(&text.text)?;
    assert_eq!(review["lint"]["success"], false);
    assert_eq!(review["source"]["files"][0]["path"], "src/lib.rs");
    assert!(
        review["lint"]["diagnostics"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    let captured = requests.lock().await;
    assert_eq!(captured.len(), 1);
    let prompt = captured[0].to_string();
    assert!(prompt.contains("incorrect"));
    assert!(prompt.contains("mismatched types"));
    drop(captured);

    std::fs::remove_file(root.join("Cargo.toml"))?;
    let failed = client
        .call_tool(
            CallToolRequestParams::new("review_rust")
                .with_arguments(arguments.as_object().context("review arguments")?.clone()),
        )
        .await;
    match failed {
        Ok(result) => {
            assert_eq!(result.is_error, Some(true), "missing Cargo.toml must fail");
            assert!(
                result.content.iter().any(|block| block
                    .as_text()
                    .is_some_and(|text| text.text.contains("Clippy failed"))),
                "lint failure must be reported"
            );
        }
        Err(error) => assert!(error.to_string().contains("Clippy failed")),
    }
    assert_eq!(
        requests.lock().await.len(),
        1,
        "failed lint must not call the model"
    );
    client.cancel().await?;
    server_task.await??;
    model_task.abort();
    pool.close().await;
    Ok(())
}
