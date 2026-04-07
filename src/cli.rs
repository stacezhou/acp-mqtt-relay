use anyhow::{bail, Result};
use clap::{Args, Parser};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Parser)]
#[command(name = "amr", version, about = "ACP-over-MQTT relay")]
pub struct Cli {
    #[command(flatten)]
    pub common: CommonConfig,

    #[arg(long, help = "Run in serve mode and bridge MQTT to a child process")]
    pub serve: bool,

    #[arg(
        long,
        requires = "serve",
        help = "Agent launch command, executed via the system shell"
    )]
    pub command: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Mode {
    User(UserConfig),
    Work(WorkConfig),
}

impl Cli {
    pub fn mode(self) -> Result<Mode> {
        if self.serve {
            let command = self
                .command
                .ok_or_else(|| anyhow::anyhow!("--command is required when --serve is set"))?;

            return Ok(Mode::Work(WorkConfig {
                common: self.common,
                command,
            }));
        }

        if self.command.is_some() {
            bail!("--command can only be used together with --serve");
        }

        Ok(Mode::User(UserConfig {
            common: self.common,
        }))
    }
}

#[derive(Debug, Clone, Args)]
pub struct CommonConfig {
    #[arg(help = "Logical node identifier used to derive MQTT topics")]
    pub node_id: String,

    #[arg(long, help = "Write amr internal logs to the given file path")]
    pub log: Option<PathBuf>,

    #[arg(long, help = "MQTT broker url, for example mqtt://localhost:1883")]
    pub broker: Option<String>,

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

#[derive(Debug, Clone, Deserialize)]
pub struct YamlConfig {
    pub broker: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Mode};

    #[test]
    fn parses_user_mode_with_positional_node_id() {
        let cli = Cli::try_parse_from([
            "amr",
            "my-agent",
            "--broker",
            "localhost",
            "--log",
            "/tmp/amr.log",
        ])
        .unwrap();
        assert_eq!(cli.common.node_id, "my-agent");
        assert_eq!(cli.common.broker.as_deref(), Some("localhost"));
        assert_eq!(
            cli.common.log.as_deref(),
            Some(std::path::Path::new("/tmp/amr.log"))
        );
        assert!(!cli.serve);
        assert!(matches!(cli.mode().unwrap(), Mode::User(_)));
    }

    #[test]
    fn parses_serve_mode_with_command() {
        let cli = Cli::try_parse_from([
            "amr",
            "--serve",
            "my-agent",
            "--broker",
            "localhost",
            "--command",
            "cat",
        ])
        .unwrap();

        match cli.mode().unwrap() {
            Mode::Work(config) => {
                assert_eq!(config.common.node_id, "my-agent");
                assert_eq!(config.common.broker.as_deref(), Some("localhost"));
                assert_eq!(config.command, "cat");
            }
            Mode::User(_) => panic!("expected work mode"),
        }
    }

    #[test]
    fn rejects_missing_node_id() {
        assert!(Cli::try_parse_from(["amr", "--broker", "localhost"]).is_err());
    }

    #[test]
    fn rejects_command_without_serve() {
        assert!(Cli::try_parse_from(["amr", "my-agent", "--command", "cat"]).is_err());
    }

    #[test]
    fn serve_requires_command() {
        let cli =
            Cli::try_parse_from(["amr", "--serve", "my-agent", "--broker", "localhost"]).unwrap();
        let error = cli.mode().unwrap_err();
        assert!(error.to_string().contains("--command"));
    }
}
