mod cli;
mod message;
mod mqtt_client;
mod user_mode;
mod work_mode;

use std::fs;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, CommonConfig, Mode, YamlConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let mut cli = Cli::parse();
    tracing::info!(?cli, "parsed cli arguments");

    let yaml_config = load_yaml_config().await?;

    merge_config(&mut cli.common, &yaml_config);
    validate_config(&cli.common)?;
    tracing::info!(
        broker = ?cli.common.broker,
        node_id = %cli.common.node_id,
        serve = cli.serve,
        "effective configuration after merge"
    );

    match cli.mode()? {
        Mode::User(config) => user_mode::run(config).await,
        Mode::Work(config) => work_mode::run(config).await,
    }
}

async fn load_yaml_config() -> Result<Option<YamlConfig>> {
    let home = home::home_dir().context("failed to get home directory")?;
    let config_root = home.join(".config");
    let primary_config_path = config_root.join("amr").join("config.yaml");
    let legacy_config_path = config_root.join("acp-mqtt-relay").join("config.yaml");
    let config_path = if primary_config_path.exists() {
        primary_config_path
    } else if legacy_config_path.exists() {
        legacy_config_path
    } else {
        tracing::debug!(
            primary = ?primary_config_path,
            legacy = ?legacy_config_path,
            "config file not found, skipping"
        );
        return Ok(None);
    };

    tracing::info!(path = ?config_path, "loading config file");
    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read config file: {:?}", config_path))?;

    let config: YamlConfig = serde_yml::from_str(&content)
        .with_context(|| format!("failed to parse yaml config: {:?}", config_path))?;

    Ok(Some(config))
}

fn merge_config(common: &mut CommonConfig, yaml: &Option<YamlConfig>) {
    if let Some(yaml) = yaml {
        if common.broker.is_none() {
            common.broker = yaml.broker.clone();
        }
        if common.username.is_none() {
            common.username = yaml.username.clone();
        }
        if common.password.is_none() {
            common.password = yaml.password.clone();
        }
    }
}

fn validate_config(common: &CommonConfig) -> Result<()> {
    if common.broker.is_none() {
        anyhow::bail!(
            "MQTT broker is not configured. Provide it via --broker or in the config file."
        );
    }
    Ok(())
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
