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
    /// Optional Discord webhook for drop claims and anything needing attention.
    pub discord_webhook: Option<String>,
    /// Route all Twitch traffic through this proxy. Applies to every account
    /// unless one overrides it.
    pub proxy: Option<String>,
    /// Timings. Every one has a working default; this is for tuning a slow
    /// connection or a Raspberry Pi, not for ordinary use.
    pub timing: Timing,
    /// Bearer token for a dashboard bound off loopback.
    ///
    /// Generated on first use and kept, rather than regenerated each start: a
    /// token that changed every restart invalidated the URL the user had just
    /// bookmarked, and left `btdm control` with no way to authenticate.
    pub dashboard_token: Option<String>,
    pub accounts: Vec<AccountConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".into(),
            data_dir: None,
            discord_webhook: None,
            proxy: None,
            timing: Timing::default(),
            dashboard_token: None,
            accounts: vec![AccountConfig::default()],
        }
    }
}

/// Every timing the engine uses, so none of them requires a rebuild to change.
///
/// The design notes said "starting values, all configurable" from the outset;
/// they were in fact unreachable from the config file entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timing {
    /// How often a watch heartbeat is sent. Twitch expects roughly one a
    /// minute; raising it earns nothing and lowering it stops progress.
    pub watch_interval_secs: u32,
    /// How often progress is polled as a backstop to the websocket.
    pub poll_interval_secs: u32,
    /// How often campaigns are re-fetched.
    pub sync_interval_mins: u32,
    /// How often the choice of channel is reconsidered while watching.
    pub reselect_interval_mins: u32,
    /// Channels to consider per game.
    pub channels_per_game: u32,
    /// Channels to verify per campaign.
    pub max_channels_per_campaign: u32,
    /// Upper bound on availability checks in one selection pass.
    pub availability_checks: u32,
    /// How many of those run at once.
    pub check_concurrency: u32,
    /// Quiet for this long is suspicious.
    pub suspect_after_secs: u32,
    /// Quiet for this long starts the recovery ladder.
    pub stalled_after_secs: u32,
    /// Minimum gap between recovery rungs.
    pub recovery_cooldown_secs: u32,
    /// How many channels to follow for stream state over the websocket.
    ///
    /// One is the channel being watched, which is the only subscription that
    /// clearly pays for itself. Raise it to hear about other channels coming
    /// online before the next scheduled re-evaluation.
    pub followed_channels: u32,
    /// How often to reconsider the channel while a campaign is at risk of
    /// being lost. Deadline pressure is exactly when a ten-minute wait costs
    /// something.
    pub urgent_reselect_mins: u32,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            watch_interval_secs: 60,
            poll_interval_secs: 120,
            sync_interval_mins: 10,
            reselect_interval_mins: 10,
            channels_per_game: 30,
            max_channels_per_campaign: 8,
            availability_checks: 160,
            check_concurrency: 8,
            suspect_after_secs: 150,
            stalled_after_secs: 360,
            recovery_cooldown_secs: 60,
            followed_channels: 1,
            urgent_reselect_mins: 2,
        }
    }
}

impl Timing {
    /// Applies these to an engine configuration.
    ///
    /// Values that would stop mining outright are clamped rather than
    /// obeyed: a zero heartbeat interval is a typo, not an instruction, and
    /// honouring it would hammer Twitch until the account was flagged.
    pub fn apply(&self, cfg: &mut btdm_core::engine::EngineConfig) {
        use chrono::Duration;
        cfg.watch_interval = Duration::seconds(self.watch_interval_secs.clamp(15, 600) as i64);
        cfg.poll_interval = Duration::seconds(self.poll_interval_secs.clamp(30, 3600) as i64);
        cfg.sync_interval = Duration::minutes(self.sync_interval_mins.clamp(1, 720) as i64);
        cfg.reselect_interval =
            Duration::minutes(self.reselect_interval_mins.clamp(1, 720) as i64);
        cfg.channel_limit = self.channels_per_game.clamp(5, 200);
        cfg.max_channels_per_campaign = self.max_channels_per_campaign.clamp(1, 50) as usize;
        cfg.max_availability_checks = self.availability_checks.clamp(5, 1000) as usize;
        cfg.check_concurrency = self.check_concurrency.clamp(1, 32) as usize;
        cfg.watchdog.suspect_after = Duration::seconds(self.suspect_after_secs.clamp(30, 3600) as i64);
        cfg.watchdog.stalled_after = Duration::seconds(self.stalled_after_secs.clamp(60, 7200) as i64);
        cfg.watchdog.cooldown =
            Duration::seconds(self.recovery_cooldown_secs.clamp(10, 3600) as i64);
        cfg.urgent_reselect_interval =
            Duration::minutes(self.urgent_reselect_mins.clamp(1, 120) as i64);

        // A stall threshold at or below the suspicion threshold would skip
        // straight to recovery on every quiet moment.
        if cfg.watchdog.stalled_after <= cfg.watchdog.suspect_after {
            cfg.watchdog.stalled_after = cfg.watchdog.suspect_after + Duration::seconds(60);
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
    /// Campaign ids never to mine.
    pub excluded_campaigns: Vec<String>,
    /// Game names never to mine.
    pub excluded_games: Vec<String>,
    /// Route this account's traffic through its own proxy, overriding the
    /// global one.
    pub proxy: Option<String>,
    /// Start paused. Set by the dashboard's pause button so the state survives
    /// a restart — a miner that silently resumed on reboot would be mining
    /// against the user's stated wish.
    pub paused: bool,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            name: "default".into(),
            strategy: "deadline".into(),
            priority_games: Vec::new(),
            favourite_channels: Vec::new(),
            pinned_channel: None,
            excluded_campaigns: Vec::new(),
            excluded_games: Vec::new(),
            proxy: None,
            paused: false,
        }
    }
}

impl AccountConfig {
    /// Folds runtime changes back in, so a setting changed on the dashboard
    /// survives a restart.
    pub fn apply(&mut self, s: &btdm_core::control::PersistedSettings) {
        self.strategy = s.strategy.clone();
        self.priority_games = s.priority_games.clone();
        self.favourite_channels = s.favourite_channels.clone();
        self.pinned_channel = s.pinned_channel.clone();
        self.excluded_campaigns = s.excluded_campaigns.clone();
        self.excluded_games = s.excluded_games.clone();
        self.paused = s.paused;
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

/// Serialises writes to `config.toml`.
///
/// Every account runs in its own task and any of them can change a setting
/// from the dashboard. Each save is a read-modify-write of the whole file, so
/// without a lock two accounts saving at once would silently discard one of
/// the changes.
#[derive(Clone)]
pub struct ConfigStore {
    path: PathBuf,
    lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl ConfigStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Writes one account's settings back, leaving every other account and
    /// every unrelated key untouched.
    pub async fn save_account(
        &self,
        account: &str,
        settings: &btdm_core::control::PersistedSettings,
    ) -> Result<(), ConfigError> {
        let _guard = self.lock.lock().await;

        // Re-read rather than mutating an in-memory copy: another account may
        // have saved since this one started.
        let mut cfg = Config::load(&self.path)?;
        match cfg.accounts.iter_mut().find(|a| a.name == account) {
            Some(existing) => existing.apply(settings),
            None => {
                let mut fresh = AccountConfig {
                    name: account.to_owned(),
                    ..AccountConfig::default()
                };
                fresh.apply(settings);
                cfg.accounts.push(fresh);
            }
        }
        cfg.save_atomic(&self.path)
    }

    /// Stores the dashboard token so it survives a restart and `btdm control`
    /// can find it.
    pub async fn save_dashboard_token(&self, token: &str) -> Result<(), ConfigError> {
        let _guard = self.lock.lock().await;
        let mut cfg = Config::load(&self.path)?;
        cfg.dashboard_token = Some(token.to_owned());
        cfg.save_atomic(&self.path)
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
            discord_webhook: None,
            proxy: None,
            timing: Timing::default(),
            dashboard_token: None,
            accounts: vec![AccountConfig {
                name: "main".into(),
                strategy: "coverage".into(),
                priority_games: vec!["Deadlock".into()],
                favourite_channels: vec!["somestreamer".into()],
                ..AccountConfig::default()
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
            discord_webhook: None,
            proxy: None,
            timing: Timing::default(),
            dashboard_token: None,
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
            discord_webhook: None,
            proxy: None,
            timing: Timing::default(),
            dashboard_token: None,
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
        assert!(!cfg.account("main").paused);
        assert!(cfg.account("main").excluded_games.is_empty());
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use btdm_core::control::PersistedSettings;

    fn settings(strategy: &str) -> PersistedSettings {
        PersistedSettings {
            strategy: strategy.into(),
            priority_games: vec!["deadlock".into()],
            favourite_channels: vec!["someone".into()],
            pinned_channel: None,
            excluded_campaigns: vec!["c1".into()],
            excluded_games: vec![],
            paused: true,
        }
    }

    #[tokio::test]
    async fn a_runtime_change_is_written_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_atomic(&path).unwrap();

        let store = ConfigStore::new(path.clone());
        store.save_account("default", &settings("coverage")).await.unwrap();

        let reloaded = Config::load(&path).unwrap();
        let a = reloaded.account("default");
        assert_eq!(a.strategy, "coverage");
        assert!(a.paused);
        assert_eq!(a.excluded_campaigns, vec!["c1"]);
    }

    #[tokio::test]
    async fn saving_an_unknown_account_adds_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_atomic(&path).unwrap();

        let store = ConfigStore::new(path.clone());
        store.save_account("alt", &settings("priority")).await.unwrap();

        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.account("alt").strategy, "priority");
        // The pre-existing account must survive.
        assert_eq!(reloaded.account("default").strategy, "deadline");
    }

    /// Two accounts saving at once must not lose one another's changes. Before
    /// the lock and the re-read, the second write was based on a stale copy.
    #[tokio::test]
    async fn concurrent_saves_do_not_lose_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_atomic(&path).unwrap();

        let store = ConfigStore::new(path.clone());
        let a = store.clone();
        let b = store.clone();

        let (ra, rb) = tokio::join!(
            tokio::spawn(async move { a.save_account("one", &settings("coverage")).await }),
            tokio::spawn(async move { b.save_account("two", &settings("priority")).await }),
        );
        ra.unwrap().unwrap();
        rb.unwrap().unwrap();

        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.account("one").strategy, "coverage");
        assert_eq!(reloaded.account("two").strategy, "priority");
    }

    /// Other keys in the file must survive a settings write.
    #[tokio::test]
    async fn unrelated_settings_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config {
            bind: "0.0.0.0:9999".into(),
            data_dir: None,
            discord_webhook: Some("https://example.invalid/hook".into()),
            proxy: None,
            timing: Timing::default(),
            dashboard_token: None,
            accounts: vec![AccountConfig::default()],
        }
        .save_atomic(&path)
        .unwrap();

        ConfigStore::new(path.clone())
            .save_account("default", &settings("manual"))
            .await
            .unwrap();

        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.bind, "0.0.0.0:9999");
        assert_eq!(
            reloaded.discord_webhook.as_deref(),
            Some("https://example.invalid/hook")
        );
    }
}
