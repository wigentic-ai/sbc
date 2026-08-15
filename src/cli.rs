use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::{BoxError, Result};

const ABOUT: &str = "Connect to Docker Sandboxes locally or over SSH";

#[derive(Debug, Parser)]
#[command(
    name = "sbc",
    version,
    about = ABOUT,
    disable_help_subcommand = true,
    after_help = "Configuration:\n  sbc config set-host <HOST>\n  sbc config show\n  sbc config path"
)]
pub(crate) struct ConnectArgs {
    /// SSH host or configured host name
    #[arg(long, short = 'H', conflicts_with = "local")]
    pub(crate) host: Option<String>,

    /// Connect through the local sbx daemon
    #[arg(long, conflicts_with = "host")]
    pub(crate) local: bool,

    /// Disable Ctrl+V clipboard bridging
    #[arg(long, env = "SBC_NO_CLIPBOARD")]
    pub(crate) no_clipboard: bool,

    /// Use a specific configuration file
    #[arg(long, env = "SBC_CONFIG", value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,

    /// Sandbox name, or HOST/SANDBOX
    pub(crate) sandbox: String,

    /// Command to run instead of reconnecting to the configured agent
    #[arg(last = true, allow_hyphen_values = true, value_name = "COMMAND")]
    pub(crate) command: Vec<OsString>,
}

#[derive(Debug, Parser)]
#[command(name = "sbc config", about = "Manage sbc configuration")]
struct ConfigCli {
    /// Use a specific configuration file
    #[arg(long, env = "SBC_CONFIG", value_name = "PATH", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Set the default SSH host
    SetHost(HostArg),
    /// Remove the default SSH host
    ClearHost,
    /// Print the resolved configuration
    Show,
    /// Print the configuration file path
    Path,
}

#[derive(Debug, Args)]
struct HostArg {
    /// SSH host alias to use by default
    host: String,
}

#[derive(Debug)]
pub(crate) enum Invocation {
    Connect(ConnectArgs),
    Config(ConfigArgs),
}

#[derive(Debug)]
pub(crate) struct ConfigArgs {
    pub(crate) config: Option<PathBuf>,
    pub(crate) action: ConfigAction,
}

#[derive(Debug)]
pub(crate) enum ConfigAction {
    SetHost(String),
    ClearHost,
    Show,
    Path,
}

pub(crate) fn parse() -> Result<Invocation> {
    parse_from(std::env::args_os())
}

fn parse_from<I, T>(args: I) -> Result<Invocation>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if args.get(1).is_some_and(|arg| arg == "config") {
        let config_args = std::iter::once(args[0].clone())
            .chain(args.into_iter().skip(2))
            .collect::<Vec<_>>();
        let parsed = ConfigCli::try_parse_from(config_args)
            .map_err(|error| -> BoxError { Box::new(error) })?;
        let action = match parsed.command {
            ConfigCommand::SetHost(arg) => ConfigAction::SetHost(arg.host),
            ConfigCommand::ClearHost => ConfigAction::ClearHost,
            ConfigCommand::Show => ConfigAction::Show,
            ConfigCommand::Path => ConfigAction::Path,
        };
        Ok(Invocation::Config(ConfigArgs {
            config: parsed.config,
            action,
        }))
    } else {
        ConnectArgs::try_parse_from(args)
            .map(Invocation::Connect)
            .map_err(|error| -> BoxError { Box::new(error) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_reconnect() {
        let Invocation::Connect(args) = parse_from(["sbc", "demo"]).unwrap() else {
            panic!("expected connect invocation");
        };
        assert_eq!(args.sandbox, "demo");
        assert!(args.command.is_empty());
    }

    #[test]
    fn parses_arbitrary_command_after_separator() {
        let Invocation::Connect(args) =
            parse_from(["sbc", "build/demo", "--", "bash", "-l"]).unwrap()
        else {
            panic!("expected connect invocation");
        };
        assert_eq!(args.sandbox, "build/demo");
        assert_eq!(args.command, ["bash", "-l"]);
    }

    #[test]
    fn parses_config_command() {
        let Invocation::Config(args) = parse_from(["sbc", "config", "set-host", "build"]).unwrap()
        else {
            panic!("expected config invocation");
        };
        assert!(matches!(args.action, ConfigAction::SetHost(host) if host == "build"));
    }
}
