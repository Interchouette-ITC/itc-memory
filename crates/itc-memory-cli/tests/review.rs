//! Exercises CLI search and Rust review using PostgreSQL and a local mock model.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, bail};
use axum::{Json, Router, extract::State, routing::post};
use itc_memory_core::DatabaseConfig;
use itc_memory_db::{connect, migrate, projects_under_roots};
use itc_memory_model::RegistryConfig;
use serde_json::{Value, json};
use sqlx::{
    Executor,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::{net::TcpListener, process::Command, sync::Mutex};
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
async fn cli_search_and_review_use_indexed_source_and_clippy() -> anyhow::Result<()> {
    itc_memory_core::load_environment()?;
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

    let root = std::env::temp_dir().join(format!("itc-cli-review-{}", Uuid::new_v4()));
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
            "review fixture",
        ],
    ] {
        if !std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .status()?
            .success()
        {
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
        if !std::process::Command::new("git")
            .args(args)
            .current_dir(&cursor)
            .status()?
            .success()
        {
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
    let projects = projects_under_roots(&pool, std::slice::from_ref(&root_string)).await?;
    let project = &projects
        .first()
        .context("missing indexed review fixture")?
        .slug;

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
    let registry_path = root.join("models.json");
    std::fs::write(&registry_path, serde_json::to_string(&registry)?)?;

    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_itc-memory"));
        command
            .current_dir(&root)
            .env("HOME", &root)
            .env("ITC_MEMORY_POSTGRES_HOST", &config.host)
            .env("ITC_MEMORY_POSTGRES_PORT", config.port.to_string())
            .env("ITC_MEMORY_POSTGRES_DB", &config.database)
            .env("ITC_MEMORY_POSTGRES_USER", &config.user)
            .env("ITC_MEMORY_POSTGRES_PASSWORD", &config.password)
            .env("PGOPTIONS", format!("-c search_path={schema}"))
            .env("ITC_MEMORY_INDEX_ROOTS", &root)
            .env_remove("ITC_MEMORY_INDEX_ROOTS_FILE")
            .env("ITC_MEMORY_MODELS_FILE", &registry_path);
        command
    };

    check_project_and_search_parity(&command, project).await?;
    check_test_generation(&command, project, &requests).await?;
    requests.lock().await.clear();

    for (query, expected_path, expected_line) in [
        (
            "pub fn answer()",
            "src/lib.rs",
            "pub fn answer() -> u32 { \"incorrect\" }",
        ),
        (
            "shared review rule",
            ".cursor/rules/review.mdc",
            "shared review rule",
        ),
    ] {
        let output = command()
            .args(["search-files", query, "--project", project, "--json"])
            .output()
            .await?;
        assert!(
            output.status.success(),
            "search failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let lines: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(lines[0]["project"], *project);
        assert_eq!(lines[0]["root_path"], root_string);
        assert_eq!(lines[0]["path"], expected_path);
        assert_eq!(lines[0]["line_number"], 1);
        assert_eq!(lines[0]["line"], expected_line);
    }

    let denied = command()
        .args([
            "lint-rust",
            project,
            "--root-path",
            "/tmp/not-allowed",
            "--json",
        ])
        .output()
        .await?;
    assert!(!denied.status.success());

    let lint = command()
        .args(["lint-rust", project, "--root-path", &root_string, "--json"])
        .output()
        .await?;
    assert!(!lint.status.success(), "invalid Rust must fail Clippy");
    let lint_result: Value = serde_json::from_slice(&lint.stdout)?;
    assert_eq!(lint_result["success"], false);
    assert!(
        lint_result["diagnostics"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );

    let review_args = [
        "review-rust",
        project,
        "--root-path",
        &root_string,
        "--file",
        "src/lib.rs",
        "--json",
    ];
    let output = command().args(review_args).output().await?;
    assert!(
        output.status.success(),
        "review failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let review: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(review["lint"]["success"], false);
    assert_eq!(review["source"]["files"][0]["path"], "src/lib.rs");
    assert_eq!(
        review["review"]["result"]["response"],
        "Review result: src/lib.rs:1"
    );
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
    let failed = command().args(review_args).output().await?;
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("Clippy failed"));
    assert_eq!(
        requests.lock().await.len(),
        1,
        "failed lint must not call the model"
    );
    model_task.abort();
    let _ = model_task.await;
    let failed = command()
        .args(["model", "generate-tests", "fn example() {}", "--json"])
        .output()
        .await?;
    assert!(!failed.status.success(), "unavailable model must fail");
    let failure: Value = serde_json::from_slice(&failed.stdout)?;
    assert!(
        failure["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    pool.close().await;
    Ok(())
}

async fn json_command(mut command: Command, args: &[&str]) -> anyhow::Result<Value> {
    let output = command.args(args).output().await?;
    anyhow::ensure!(
        output.status.success(),
        "command {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

async fn check_project_and_search_parity(
    command: &(impl Fn() -> Command + Sync),
    project: &str,
) -> anyhow::Result<()> {
    let list = json_command(
        command(),
        &[
            "project",
            "list",
            "--project",
            project,
            "--limit",
            "1",
            "--json",
        ],
    )
    .await?;
    assert_eq!(
        list.as_array()
            .context("project list is not an array")?
            .len(),
        1
    );
    assert_eq!(list[0]["slug"], project);
    let absent = json_command(
        command(),
        &["project", "list", "--project", "not-a-project", "--json"],
    )
    .await?;
    assert_eq!(absent, json!([]));
    let invalid = command()
        .args(["project", "list", "--limit", "0"])
        .output()
        .await?;
    assert!(!invalid.status.success());

    let search = json_command(
        command(),
        &["search-code", "answer", "--project", project, "--json"],
    )
    .await?;
    for key in ["symbols", "dependencies", "indexed_matches"] {
        assert!(search[key].is_array(), "missing {key} results");
    }
    assert!(
        search["symbols"]
            .as_array()
            .context("symbols")?
            .iter()
            .any(|symbol| symbol["qualified_name"]
                .as_str()
                .is_some_and(|name| name.contains("answer")))
    );
    let invalid = command()
        .args(["search-code", " ", "--json"])
        .output()
        .await?;
    assert!(!invalid.status.success());

    let checkpoint = json_command(
        command(),
        &[
            "checkpoint",
            "record",
            project,
            "--title",
            "test checkpoint",
            "--summary",
            "checkpoint summary",
            "--json",
        ],
    )
    .await?;
    Uuid::parse_str(
        checkpoint["checkpoint_id"]
            .as_str()
            .context("checkpoint ID")?,
    )?;
    let decision = json_command(
        command(),
        &[
            "decision",
            "record",
            project,
            "--title",
            "test decision",
            "--rationale",
            "test rationale",
            "--json",
        ],
    )
    .await?;
    Uuid::parse_str(decision["decision_id"].as_str().context("decision ID")?)?;
    let context = json_command(command(), &["project", "context", project, "--json"]).await?;
    assert_eq!(context["project"]["slug"], project);
    for key in ["repositories", "plan", "decisions", "checkpoints"] {
        assert!(context[key].is_array(), "missing {key}");
    }
    assert_eq!(context["decisions"][0]["title"], "test decision");
    assert_eq!(context["checkpoints"][0]["summary"], "checkpoint summary");
    assert!(context["checkpoints"][0].get("repository_state").is_none());
    Ok(())
}

async fn check_test_generation(
    command: &(impl Fn() -> Command + Sync),
    project: &str,
    requests: &Mutex<Vec<Value>>,
) -> anyhow::Result<()> {
    let output = json_command(
        command(),
        &[
            "model",
            "generate-tests",
            "fn example() {}",
            "--project",
            project,
            "--json",
        ],
    )
    .await?;
    assert_eq!(output["result"]["response"], "Review result: src/lib.rs:1");
    let captured = requests.lock().await;
    assert_eq!(captured.len(), 1);
    let content = captured[0]["messages"][1]["content"]
        .as_str()
        .context("model user prompt")?;
    assert!(content.contains("Suggest focused tests for this code. Do not claim they were run:\n```\nfn example() {}\n```"));
    assert!(content.contains("\"task\":\"test conventions and coverage for this source\""));
    drop(captured);
    let invalid = command()
        .args(["model", "generate-tests", " "])
        .output()
        .await?;
    assert!(!invalid.status.success());
    Ok(())
}
