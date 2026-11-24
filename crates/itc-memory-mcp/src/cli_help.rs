//! MCP discovery for the installed CLI.

use std::{path::PathBuf, time::Duration};

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerConfig},
    schemars::{self, JsonSchema},
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::{cancellable, internal_error};

/// Minimal MCP server that tells agents how to use the CLI.
#[derive(Clone)]
pub struct CliHelpServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl Default for CliHelpServer {
    fn default() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CliHelpInput {
    /// Optional command path, for example `["context", "build"]`.
    command: Option<Vec<String>>,
}

async fn cli_help(
    input: CliHelpInput,
    request_context: RequestContext<RoleServer>,
) -> Result<String, ErrorData> {
    let path = input.command.unwrap_or_default();
    if path.len() > 3
        || path.iter().any(|part| {
            part.is_empty()
                || part.len() > 64
                || !part.as_bytes()[0].is_ascii_lowercase()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err(ErrorData::invalid_params(
            "command must contain up to three lowercase command names",
            None,
        ));
    }
    let sibling = std::env::current_exe()
        .map_err(internal_error)?
        .with_file_name(format!("itc-memory{}", std::env::consts::EXE_SUFFIX));
    let executable = if sibling.is_file() {
        sibling
    } else {
        PathBuf::from("itc-memory")
    };
    let mut command = tokio::process::Command::new(executable);
    command.args(&path).arg("--help").kill_on_drop(true);
    let output = cancellable(
        &request_context.ct,
        tokio::time::timeout(Duration::from_secs(10), command.output()),
    )
    .await?
    .map_err(|_| internal_error("CLI help timed out"))?
    .map_err(|error| internal_error(format!("could not run installed itc-memory CLI: {error}")))?;
    if !output.status.success() {
        return Err(ErrorData::invalid_params(
            format!(
                "CLI help failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            None,
        ));
    }
    if output.stdout.len() > 64_000 {
        return Err(internal_error("CLI help exceeds the response size limit"));
    }
    let help = String::from_utf8(output.stdout).map_err(internal_error)?;
    Ok(format!(
        "Use the installed itc-memory CLI through the shell. Use --json where supported. \
         Request a command path for detailed help.\n\n{help}"
    ))
}

#[tool_router]
impl CliHelpServer {
    #[tool(
        name = "get_cli_help",
        description = "Explain how to use the installed itc-memory CLI; optionally request a command's help."
    )]
    async fn get_cli_help(
        &self,
        Parameters(input): Parameters<CliHelpInput>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        cli_help(input, request_context).await
    }
}

#[tool_handler]
#[allow(clippy::unused_async_trait_impl)]
impl ServerHandler for CliHelpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Use get_cli_help to discover the installed itc-memory CLI, then run its commands through the shell.")
    }
}
