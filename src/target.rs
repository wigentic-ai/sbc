use crate::Result;
use crate::cli::ConnectArgs;
use crate::config::{Config, validate_host_name};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Target {
    pub(crate) sandbox: String,
    pub(crate) location: Location,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Location {
    Local {
        sbx_command: String,
    },
    Remote {
        ssh_host: String,
        sbx_command: String,
    },
}

impl Target {
    pub(crate) fn resolve(args: &ConnectArgs, config: &Config) -> Result<Self> {
        let (explicit_host, sandbox) = match args.sandbox.split_once('/') {
            Some((host, sandbox)) => (Some(host), sandbox),
            None => (None, args.sandbox.as_str()),
        };
        validate_sandbox_name(sandbox)?;

        if args.local && explicit_host.is_some() {
            return Err("--local cannot be combined with HOST/SANDBOX".into());
        }
        if args.host.is_some() && explicit_host.is_some() {
            return Err("--host cannot be combined with HOST/SANDBOX".into());
        }

        let host = if args.local {
            None
        } else {
            explicit_host
                .map(str::to_owned)
                .or_else(|| args.host.clone())
                .or_else(|| std::env::var("SBC_HOST").ok())
                .or_else(|| config.default_host.clone())
        };

        let location = match host.as_deref() {
            None | Some("local") => Location::Local {
                sbx_command: "sbx".into(),
            },
            Some(host_name) => {
                validate_host_name(host_name)?;
                let host_config = config.hosts.get(host_name);
                Location::Remote {
                    ssh_host: host_config
                        .and_then(|host| host.ssh_host.clone())
                        .unwrap_or_else(|| host_name.to_owned()),
                    sbx_command: host_config
                        .and_then(|host| host.sbx_command.clone())
                        .unwrap_or_else(|| "sbx".into()),
                }
            }
        };

        Ok(Self {
            sandbox: sandbox.to_owned(),
            location,
        })
    }
}

fn validate_sandbox_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".+-".contains(&byte))
    {
        return Err(format!("invalid sandbox name: {name:?}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ConnectArgs;
    use crate::config::HostConfig;
    use std::collections::BTreeMap;

    fn args(name: &str) -> ConnectArgs {
        ConnectArgs {
            host: None,
            local: false,
            no_clipboard: false,
            config: None,
            sandbox: name.into(),
            command: vec![],
        }
    }

    #[test]
    fn defaults_to_local_without_configuration() {
        let target = Target::resolve(&args("demo"), &Config::default()).unwrap();
        assert!(matches!(target.location, Location::Local { .. }));
    }

    #[test]
    fn uses_default_remote_host() {
        let config = Config {
            default_host: Some("build".into()),
            ..Config::default()
        };
        let target = Target::resolve(&args("demo"), &config).unwrap();
        assert!(matches!(
            target.location,
            Location::Remote { ref ssh_host, .. } if ssh_host == "build"
        ));
    }

    #[test]
    fn explicit_target_uses_host_configuration() {
        let mut hosts = BTreeMap::new();
        hosts.insert(
            "build".into(),
            HostConfig {
                ssh_host: Some("linux.example".into()),
                sbx_command: Some("/usr/bin/sbx".into()),
            },
        );
        let config = Config {
            default_host: None,
            hosts,
        };
        let target = Target::resolve(&args("build/demo"), &config).unwrap();
        assert_eq!(
            target.location,
            Location::Remote {
                ssh_host: "linux.example".into(),
                sbx_command: "/usr/bin/sbx".into()
            }
        );
    }

    #[test]
    fn explicit_local_target_ignores_default_host() {
        let config = Config {
            default_host: Some("build".into()),
            ..Config::default()
        };
        let target = Target::resolve(&args("local/demo"), &config).unwrap();
        assert!(matches!(target.location, Location::Local { .. }));
    }
}
