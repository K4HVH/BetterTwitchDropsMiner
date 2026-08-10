//! Settings live in TOML so Docker and systemd deployments stay declarative.
//! Runtime state lives in SQLite instead (see [`crate::storage`]).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config: {0}")]
    Io(#[from] std::io::Error),
    #[error("parsing config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("serialising config: {0}")]
    Serialise(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Loopback by default. Any other bind address will require a bearer token
    /// once the dashboard lands, because the state DB holds live OAuth tokens.
    pub bind: String,
    pub data_dir: Option<PathBuf>,
    pub accounts: Vec<AccountConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".into(),
            data_dir: None,
            accounts: vec![AccountConfig::default()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountConfig {
    pub name: String,
    /// One of: deadline, priority, coverage, completionist, manual.
    pub strategy: String,
    /// Game names in priority order. Used by the `priority` strategy.
    pub priority_games: Vec<String>,
    /// Channel logins preferred when the choice is otherwise a tie. Applies
    /// under every strategy.
    pub favourite_channels: Vec<String>,
    /// Channel login to watch exclusively, under the `manual` strategy.
    pub pinned_channel: Option<String>,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            name: "default".into(),
            strategy: "deadline".into(),
            priority_games: Vec::new(),
            favourite_channels: Vec::new(),
            pinned_channel: None,
        }
    }
}

impl Config {
    /// Settings for one account, falling back to defaults when the account is
    /// not described in the file.
    pub fn account(&self, name: &str) -> AccountConfig {
        self.accounts
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .unwrap_or_else(|| AccountConfig {
                name: name.to_owned(),
                ..AccountConfig::default()
            })
    }
}

impl Config {
    /// A missing file is not an error — first run starts from defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write to a temp file, fsync, then rename. A crash mid-write leaves the
    /// previous config intact rather than a truncated one.
    pub fn save_atomic(&self, path: &Path) -> Result<(), ConfigError> {
        use std::io::Write;

        let text = toml::to_string_pretty(self)?;
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;

        let tmp = path.with_extension("toml.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:8080");
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config {
            bind: "0.0.0.0:9000".into(),
            data_dir: None,
            accounts: vec![AccountConfig {
                name: "main".into(),
                strategy: "coverage".into(),
                priority_games: vec!["Deadlock".into()],
                favourite_channels: vec!["somestreamer".into()],
                pinned_channel: None,
            }],
        };
        cfg.save_atomic(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_atomic(&path).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn config_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_atomic(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[cfg(test)]
mod account_tests {
    use super::*;

    #[test]
    fn an_unconfigured_account_gets_sensible_defaults() {
        let cfg = Config {
            bind: "127.0.0.1:8080".into(),
            data_dir: None,
            accounts: vec![],
        };
        let a = cfg.account("whoever");
        assert_eq!(a.name, "whoever");
        assert_eq!(a.strategy, "deadline");
        assert!(a.priority_games.is_empty());
    }

    #[test]
    fn a_configured_account_is_found_by_name() {
        let cfg = Config {
            bind: "127.0.0.1:8080".into(),
            data_dir: None,
            accounts: vec![
                AccountConfig {
                    name: "alt".into(),
                    strategy: "coverage".into(),
                    ..AccountConfig::default()
                },
                AccountConfig {
                    name: "main".into(),
                    strategy: "priority".into(),
                    ..AccountConfig::default()
                },
            ],
        };
        assert_eq!(cfg.account("main").strategy, "priority");
        assert_eq!(cfg.account("alt").strategy, "coverage");
    }

    /// Old config files predate the strategy field; they must keep working
    /// rather than failing to parse.
    #[test]
    fn a_config_without_the_newer_fields_still_loads() {
        let toml = r#"
bind = "127.0.0.1:8080"

[[accounts]]
name = "main"
priority_games = ["Deadlock"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.account("main").strategy, "deadline");
        assert_eq!(cfg.account("main").priority_games, vec!["Deadlock"]);
    }
}
