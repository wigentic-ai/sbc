use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::cli::{ConfigAction, ConfigArgs};
use crate::{BoxError, Result};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) default_host: Option<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) hosts: BTreeMap<String, HostConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ssh_host: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sbx_command: Option<String>,
}

impl Config {
    pub(crate) fn load(explicit_path: Option<&Path>) -> Result<Self> {
        let path = explicit_path
            .map(Path::to_path_buf)
            .unwrap_or(config_path()?);
        match fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents)
                .map_err(|error| format!("could not parse {}: {error}", path.display()).into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("could not read {}: {error}", path.display()).into()),
        }
    }

    fn save(&self, explicit_path: Option<&Path>) -> Result<PathBuf> {
        let path = explicit_path
            .map(Path::to_path_buf)
            .unwrap_or(config_path()?);
        let parent = path
            .parent()
            .ok_or_else(|| format!("configuration path has no parent: {}", path.display()))?;
        fs::create_dir_all(parent)?;

        let contents = toml::to_string_pretty(self)?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        let mut file = fs::File::create(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;

        // Unix replaces the destination atomically. Windows rename does not,
        // so remove the old file immediately before moving the completed one.
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(&temporary, &path)?;
        Ok(path)
    }
}

pub(crate) fn run(args: ConfigArgs) -> Result<()> {
    match args.action {
        ConfigAction::Path => println!("{}", resolved_path(args.config.as_deref())?.display()),
        ConfigAction::Show => {
            let config = Config::load(args.config.as_deref())?;
            print!("{}", toml::to_string_pretty(&config)?);
        }
        ConfigAction::SetHost(host) => {
            validate_host_name(&host)?;
            let mut config = Config::load(args.config.as_deref())?;
            config.default_host = Some(host);
            let path = config.save(args.config.as_deref())?;
            println!("Default sandbox host saved to {}", path.display());
        }
        ConfigAction::ClearHost => {
            let mut config = Config::load(args.config.as_deref())?;
            config.default_host = None;
            let path = config.save(args.config.as_deref())?;
            println!("Default sandbox host cleared in {}", path.display());
        }
    }
    Ok(())
}

pub(crate) fn validate_host_name(host: &str) -> Result<()> {
    if host.is_empty()
        || host.starts_with('-')
        || host.contains('/')
        || host.contains(char::is_whitespace)
    {
        return Err(format!("invalid SSH host name: {host:?}").into());
    }
    Ok(())
}

fn resolved_path(explicit_path: Option<&Path>) -> Result<PathBuf> {
    explicit_path
        .map(Path::to_path_buf)
        .map_or_else(config_path, Ok)
}

fn config_path() -> Result<PathBuf> {
    BaseDirs::new()
        .map(|dirs| dirs.config_dir().join("sbc").join("config.toml"))
        .ok_or_else(|| -> BoxError {
            "could not determine the user configuration directory".into()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "sbc-config-test-{}-{}.toml",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn missing_config_is_empty() {
        let path = temporary_path();
        assert!(Config::load(Some(&path)).unwrap().default_host.is_none());
    }

    #[test]
    fn saves_and_loads_default_host() {
        let path = temporary_path();
        let mut config = Config {
            default_host: Some("build".into()),
            ..Config::default()
        };
        config.save(Some(&path)).unwrap();
        assert_eq!(
            Config::load(Some(&path)).unwrap().default_host.as_deref(),
            Some("build")
        );
        config.default_host = Some("other".into());
        config.save(Some(&path)).unwrap();
        assert_eq!(
            Config::load(Some(&path)).unwrap().default_host.as_deref(),
            Some("other")
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_option_shaped_host() {
        assert!(validate_host_name("-oProxyCommand=bad").is_err());
    }
}
