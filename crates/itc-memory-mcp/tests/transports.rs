//! End-to-end contract checks for both MCP transports.

use std::{
    collections::HashMap, fs, net::SocketAddr, path::PathBuf, process::Stdio, time::Duration,
};

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

#[cfg(unix)]
struct Fixture(PathBuf);

#[cfg(unix)]
impl Fixture {
    fn new() -> anyhow::Result<Self> {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("itc-mcp-help-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root)?;
        fs::copy(
            env!("CARGO_BIN_EXE_itc-memory-mcp"),
            root.join("itc-memory-mcp"),
        )?;
        let cli = root.join("itc-memory");
        fs::write(&cli, "#!/bin/sh\nprintf 'CLI help: %s\\n' \"$*\"\n")?;
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o755))?;
        Ok(Self(root))
    }

    fn server(&self) -> PathBuf {
        self.0.join("itc-memory-mcp")
    }
}

#[cfg(unix)]
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove MCP help fixture");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_and_streamable_http_expose_the_same_mcp_tool_contract() -> anyhow::Result<()> {
    let fixture = Fixture::new()?;

    let mut stdio_command = Command::new(fixture.server());
    stdio_command
        .current_dir(&fixture.0)
        .env("HOME", &fixture.0)
        .env("ITC_MEMORY_MODELS_FILE", "/nonexistent/models.json")
        .arg("--transport")
        .arg("stdio");
    let stdio_transport = TokioChildProcess::new(stdio_command)?;
    let stdio_client = ().serve(stdio_transport).await?;
    let stdio_tools = stdio_client.list_all_tools().await?;
    let stdio_help = stdio_client
        .call_tool(
            CallToolRequestParams::new("get_cli_help").with_arguments(
                serde_json::json!({"command": ["context", "build"]})
                    .as_object()
                    .context("invalid test arguments")?
                    .clone(),
            ),
        )
        .await?;
    let invalid = stdio_client
        .call_tool(
            CallToolRequestParams::new("get_cli_help").with_arguments(
                serde_json::json!({"command": ["--version"]})
                    .as_object()
                    .context("invalid test arguments")?
                    .clone(),
            ),
        )
        .await;
    assert!(invalid.is_err());
    stdio_client.cancel().await?;

    let address = reserve_loopback_address().await?;
    let mut http_command = Command::new(fixture.server());
    http_command
        .current_dir(&fixture.0)
        .env("HOME", &fixture.0)
        .env("ITC_MEMORY_MODELS_FILE", "/nonexistent/models.json")
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
    let http_help = http_client
        .call_tool(
            CallToolRequestParams::new("get_cli_help").with_arguments(
                serde_json::json!({"command": ["context", "build"]})
                    .as_object()
                    .context("invalid test arguments")?
                    .clone(),
            ),
        )
        .await?;
    http_client.cancel().await?;
    if http_process.try_wait()?.is_none() {
        http_process.kill().await?;
    }
    http_process.wait().await?;

    let contract = tool_contract(stdio_tools)?;
    assert_eq!(contract, tool_contract(http_tools)?);
    assert_eq!(contract.len(), 1);
    assert_eq!(contract[0].0, "get_cli_help");
    for result in [&stdio_help, &http_help] {
        assert_ne!(result.is_error, Some(true));
        let text = result
            .content
            .first()
            .and_then(|content| content.as_text())
            .context("get_cli_help did not return text")?;
        assert!(text.text.contains("CLI help: context build --help"));
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
