//! Command-line entry point for the shared MCP server.

use anyhow::Context;
use clap::{Parser, ValueEnum};
use itc_memory_mcp::{CliHelpServer, http_settings_from_environment, run_http, run_stdio};

#[derive(Debug, Parser)]
#[command(version, about = "itc-memory MCP server")]
struct Args {
    #[arg(long, value_enum, default_value_t = Transport::Stdio)]
    transport: Transport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Transport {
    Stdio,
    Http,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    itc_memory_core::load_environment().context("could not load environment configuration")?;
    let server = CliHelpServer::default();

    match args.transport {
        Transport::Stdio => run_stdio(server).await?,
        Transport::Http => {
            let (bind, token) = http_settings_from_environment()?;
            run_http(server, bind, token).await?;
        }
    }
    Ok(())
}
