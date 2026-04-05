mod cli;
mod message;
mod mqtt_client;
mod user_mode;
mod work_mode;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Mode};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let cli = Cli::parse();
    tracing::info!(?cli, "parsed cli arguments");

    match cli.mode {
        Mode::User(config) => user_mode::run(config).await,
        Mode::Work(config) => work_mode::run(config).await,
    }
}

fn init_logging() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,rumqttc=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .init();
}
