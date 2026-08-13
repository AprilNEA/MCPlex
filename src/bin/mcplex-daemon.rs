use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "mcplex-daemon",
    version,
    about = "Run the local MCPlex gateway daemon"
)]
struct Cli {
    /// Use a config file other than the platform default.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Keep the process attached to this terminal.
    #[arg(long)]
    foreground: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    mcplex::server::serve_path(cli.config).await
}
