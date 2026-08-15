use std::ffi::{OsStr, OsString};

use portable_pty::CommandBuilder;

use crate::Result;
use crate::target::{Location, Target};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionCommand {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
}

impl SessionCommand {
    pub(crate) fn for_target(target: &Target, command: &[OsString]) -> Result<Self> {
        match &target.location {
            Location::Local { sbx_command } => Ok(Self {
                program: sbx_command.into(),
                args: sbx_args(&target.sandbox, command),
            }),
            Location::Remote {
                ssh_host,
                sbx_command,
            } => {
                let remote = remote_sbx_command(sbx_command, &target.sandbox, command)?;
                Ok(Self {
                    program: "ssh".into(),
                    args: vec!["-t".into(), ssh_host.into(), remote.into()],
                })
            }
        }
    }

    pub(crate) fn builder(&self) -> CommandBuilder {
        let mut builder = CommandBuilder::new(&self.program);
        builder.args(&self.args);
        builder
    }
}

fn sbx_args(sandbox: &str, command: &[OsString]) -> Vec<OsString> {
    if command.is_empty() {
        vec!["run".into(), "--name".into(), sandbox.into()]
    } else {
        let mut args = vec!["exec".into(), "-it".into(), sandbox.into()];
        args.extend_from_slice(command);
        args
    }
}

fn remote_sbx_command(sbx: &str, sandbox: &str, command: &[OsString]) -> Result<String> {
    let mut words = vec![
        quote(sbx),
        if command.is_empty() { "run" } else { "exec" }.into(),
    ];
    if command.is_empty() {
        words.extend(["--name".into(), quote(sandbox)]);
    } else {
        words.extend(["-it".into(), quote(sandbox)]);
        for argument in command {
            let argument = argument
                .to_str()
                .ok_or("remote command arguments must be valid UTF-8")?;
            words.push(quote(argument));
        }
    }
    Ok(words.join(" "))
}

pub(crate) fn quote(value: impl AsRef<OsStr>) -> String {
    let value = value.as_ref().to_string_lossy();
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return value.into_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnects_to_agent_by_default() {
        let target = Target {
            sandbox: "demo".into(),
            location: Location::Local {
                sbx_command: "sbx".into(),
            },
        };
        let command = SessionCommand::for_target(&target, &[]).unwrap();
        assert_eq!(command.program, "sbx");
        assert_eq!(command.args, ["run", "--name", "demo"]);
    }

    #[test]
    fn runs_command_inside_remote_sandbox() {
        let target = Target {
            sandbox: "demo".into(),
            location: Location::Remote {
                ssh_host: "build".into(),
                sbx_command: "/usr/bin/sbx".into(),
            },
        };
        let command = SessionCommand::for_target(
            &target,
            &["sh".into(), "-c".into(), "printf '%s' hello".into()],
        )
        .unwrap();
        assert_eq!(command.program, "ssh");
        assert_eq!(command.args[0], "-t");
        assert_eq!(command.args[1], "build");
        assert_eq!(
            command.args[2],
            "/usr/bin/sbx exec -it demo sh -c 'printf '\"'\"'%s'\"'\"' hello'"
        );
    }

    #[test]
    fn shell_quote_handles_apostrophes() {
        assert_eq!(quote("isn't"), "'isn'\"'\"'t'");
    }
}
