//! Command-line entry point for the shared MCP server.

use anyhow::Context;
use clap::{Parser, ValueEnum};
use itc_memory_core::DatabaseConfig;
use itc_memory_db::{check_connection, connect};
use itc_memory_mcp::{McpSettings, ToolServer, run_http, run_stdio};
use itc_memory_model::ModelRegistry;

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
    let config_dir =
        itc_memory_core::load_environment().context("could not load environment configuration")?;

    let args = Args::parse();
    let settings =
        McpSettings::from_environment_in(args.transport == Transport::Http, config_dir.as_deref())?;
    let database = DatabaseConfig::from_environment()?;
    let models = ModelRegistry::from_environment_in(config_dir.as_deref())?;
    let pool = connect(&database)
        .await
        .context("could not connect to PostgreSQL")?;
    check_connection(&pool)
        .await
        .context("PostgreSQL health check failed")?;
    let server = ToolServer::new(
        pool,
        settings.roots.clone(),
        settings.allow_writes,
        settings.allow_hosted,
        models,
    );

    match args.transport {
        Transport::Stdio => run_stdio(server).await?,
        Transport::Http => {
            let token = settings
                .http_token
                .context("HTTP transport requires ITC_MEMORY_MCP_TOKEN")?;
            let bind = settings
                .http_bind
                .context("HTTP transport requires a bind address")?;
            run_http(server, bind, token).await?;
        }
    }
    Ok(())
}
