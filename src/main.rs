use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use protium_agent::{app, config::Config};

#[derive(Debug, Parser)]
#[command(name = "1h-agent", version, about)]
struct Cli {
    /// Directory the agent is allowed to access.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,

    /// Optional TOML configuration path.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let workspace = cli
        .workspace
        .canonicalize()
        .with_context(|| format!("cannot open workspace {}", cli.workspace.display()))?;
    let config = Config::load(cli.config.as_deref(), &workspace)?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "protium_agent=info".into()),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    app::run(workspace, config).await
}
