//! End-to-end contract checks for both MCP transports.

use std::{collections::HashMap, net::SocketAddr, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, bail};
use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio::{net::TcpStream, process::Command, time::sleep};

const TEST_TOKEN: &str = "phase-six-transport-test-token-123456";

#[tokio::test]
async fn stdio_and_streamable_http_expose_the_same_mcp_tool_contract() -> anyhow::Result<()> {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .context("MCP crate is not under the workspace root")?;
    let binary = env!("CARGO_BIN_EXE_itc-memory-mcp");
    let models = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models.json");

    let mut stdio_command = Command::new(binary);
    stdio_command
        .current_dir(repository_root)
        .env("ITC_MEMORY_INDEX_ROOTS", repository_root)
        .env("ITC_MEMORY_MODELS_FILE", &models)
        .env("ITC_MEMORY_MCP_ALLOW_WRITES", "false")
        .arg("--transport")
        .arg("stdio");
    let stdio_transport = TokioChildProcess::new(stdio_command)?;
    let stdio_client = ().serve(stdio_transport).await?;
    let stdio_tools = stdio_client.list_all_tools().await?;
    let stdio_projects = stdio_client
        .call_tool(
            CallToolRequestParams::new("search_projects").with_arguments(serde_json::Map::new()),
        )
        .await?;
    stdio_client.cancel().await?;

    let address = reserve_loopback_address().await?;
    let mut http_command = Command::new(binary);
    http_command
        .current_dir(repository_root)
        .env("ITC_MEMORY_INDEX_ROOTS", repository_root)
        .env("ITC_MEMORY_MODELS_FILE", &models)
        .env("ITC_MEMORY_MCP_ALLOW_WRITES", "false")
        .env("ITC_MEMORY_MCP_TOKEN", TEST_TOKEN)
        .env("ITC_MEMORY_MCP_BIND", address.to_string())
        .arg("--transport")
        .arg("http")
        .stdout(Stdio::null());
    let mut http_process = http_command.spawn()?;
    wait_until_listening(address, &mut http_process).await?;

    let config = StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp"))
        .custom_headers(HashMap::from([(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer phase-six-transport-test-token-123456"),
        )]));
    let http_client = ().serve(StreamableHttpClientTransport::from_config(config)).await?;
    let http_tools = http_client.list_all_tools().await?;
    let http_projects = http_client
        .call_tool(
            CallToolRequestParams::new("search_projects").with_arguments(serde_json::Map::new()),
        )
        .await?;
    http_client.cancel().await?;
    if http_process.try_wait()?.is_none() {
        http_process.kill().await?;
    }
    http_process.wait().await?;

    assert_eq!(tool_contract(stdio_tools)?, tool_contract(http_tools)?);
    for result in [&stdio_projects, &http_projects] {
        assert_ne!(result.is_error, Some(true));
        let text = result
            .content
            .first()
            .and_then(|content| content.as_text())
            .context("search_projects did not return text")?;
        let projects: serde_json::Value = serde_json::from_str(&text.text)?;
        assert!(
            projects.is_array(),
            "search_projects must return a JSON array"
        );
    }
    Ok(())
}

fn tool_contract(
    tools: Vec<rmcp::model::Tool>,
) -> anyhow::Result<Vec<(String, serde_json::Value)>> {
    let mut contract = tools
        .into_iter()
        .map(|tool| {
            Ok((
                tool.name.to_string(),
                serde_json::to_value(tool.input_schema.as_ref())?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    contract.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(contract)
}

async fn reserve_loopback_address() -> anyhow::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    drop(listener);
    Ok(address)
}

async fn wait_until_listening(
    address: SocketAddr,
    child: &mut tokio::process::Child,
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = child.try_wait()? {
                bail!("HTTP MCP server exited during startup with {status}");
            }
            if TcpStream::connect(address).await.is_ok() {
                return Ok(());
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("timed out waiting for HTTP MCP server")?
}
