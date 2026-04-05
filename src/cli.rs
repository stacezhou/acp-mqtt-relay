use clap::{Args, Parser, Subcommand};

#[derive(Debug, Clone, Parser)]
#[command(name = "acp-mqtt-relay", version, about = "ACP-over-MQTT relay")]
pub struct Cli {
    #[command(subcommand)]
    pub mode: Mode,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Mode {
    User(UserConfig),
    Work(WorkConfig),
}

#[derive(Debug, Clone, Args)]
pub struct CommonConfig {
    #[arg(long, help = "MQTT broker url, for example mqtt://localhost:1883")]
    pub broker: String,

    #[arg(long, help = "Logical node identifier used to derive MQTT topics")]
    pub node_id: String,

    #[arg(long, help = "MQTT username")]
    pub username: Option<String>,

    #[arg(long, help = "MQTT password")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct UserConfig {
    #[command(flatten)]
    pub common: CommonConfig,
}

#[derive(Debug, Clone, Args)]
pub struct WorkConfig {
    #[command(flatten)]
    pub common: CommonConfig,

    #[arg(long, help = "Agent launch command, executed via the system shell")]
    pub command: String,
}
