//! Shared state between the miners and the dashboard.
//!
//! Each account's task publishes a snapshot and broadcasts events; the web
//! layer reads those and sends commands back. Accounts never see each other's
//! state, which is what keeps them independent.
//!
//! The command path is deliberately a bounded per-account channel rather than
//! shared mutable state: a miner applies a command between ticks, at a point
//! where it is safe to abandon the current channel, and nothing outside the
//! engine can mutate it mid-selection.

use btdm_core::control::Command;
use serde::Serialize;
use btdm_core::event::MinerEvent;
use btdm_core::snapshot::Snapshot;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc};

/// An event tagged with the account that produced it.
#[derive(Debug, Clone)]
pub struct AccountEvent {
    pub account: String,
    pub event: MinerEvent,
}

/// One line of stored history.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryRow {
    pub at: String,
    pub summary: String,
    pub severity: String,
}

/// One reward this account has actually earned.
#[derive(Debug, Clone, Serialize)]
pub struct RewardRow {
    pub at: String,
    pub name: String,
}

/// Where the dashboard reads history from.
///
/// The miner already writes every event to SQLite; without this the page had
/// no way to read any of it back, so opening the dashboard after a restart
/// showed an empty log and no record of anything ever claimed.
pub trait History: Send + Sync {
    fn recent(&self, account: &str, limit: u32) -> Vec<HistoryRow>;
    fn rewards(&self, account: &str, limit: u32) -> Vec<RewardRow>;
}

/// A device-code login in progress, or its outcome.
///
/// A dead token used to need shell access to fix, which is a poor answer on
/// the headless box this project targets.
#[derive(Debug, Clone, Serialize)]
pub struct LoginState {
    pub account: String,
    /// One of: pending, granted, failed.
    pub status: String,
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub detail: Option<String>,
}

/// Starts a login. Completion is reported through [`Hub::set_login_state`].
pub trait Enrolment: Send + Sync {
    fn begin(&self, account: &str) -> Result<(), String>;
}

/// What the dashboard streams: either a new snapshot or a log line.
#[derive(Debug, Clone)]
pub enum Update {
    Snapshot(Box<Snapshot>),
    Event(AccountEvent),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommandError {
    #[error("no account named '{0}' is running")]
    UnknownAccount(String),
    #[error("the miner for '{0}' is not accepting commands")]
    NotAccepting(String),
}

#[derive(Clone)]
pub struct Hub {
    /// Ordered so the dashboard lists accounts consistently between refreshes.
    snapshots: Arc<RwLock<BTreeMap<String, Snapshot>>>,
    commands: Arc<RwLock<BTreeMap<String, mpsc::Sender<Command>>>>,
    updates: broadcast::Sender<Update>,
    history: Arc<RwLock<Option<Arc<dyn History>>>>,
    enrolment: Arc<RwLock<Option<Arc<dyn Enrolment>>>>,
    logins: Arc<RwLock<BTreeMap<String, LoginState>>>,
    /// Why an account's miner stopped, so the dashboard can offer a fix rather
    /// than simply showing nothing.
    errors: Arc<RwLock<BTreeMap<String, String>>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Self {
        let (updates, _) = broadcast::channel(256);
        Self {
            snapshots: Arc::new(RwLock::new(BTreeMap::new())),
            commands: Arc::new(RwLock::new(BTreeMap::new())),
            updates,
            history: Arc::new(RwLock::new(None)),
            enrolment: Arc::new(RwLock::new(None)),
            logins: Arc::new(RwLock::new(BTreeMap::new())),
            errors: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Publishes a snapshot *and* pushes it to every open dashboard.
    ///
    /// Pushing the whole snapshot is what makes the page feel live. It
    /// previously received only an event name and had to re-fetch, so a change
    /// took a round trip to show up — and any change that produced no event
    /// (most of them) waited for the next 15-second poll.
    pub fn publish(&self, snapshot: Snapshot) {
        {
            let mut guard = self.snapshots.write().unwrap_or_else(|e| e.into_inner());
            guard.insert(snapshot.account.clone(), snapshot.clone());
        }
        let _ = self.updates.send(Update::Snapshot(Box::new(snapshot)));
    }

    /// Broadcasting with no subscribers is not an error — nobody has the
    /// dashboard open.
    pub fn emit(&self, account: &str, event: MinerEvent) {
        let _ = self.updates.send(Update::Event(AccountEvent {
            account: account.to_owned(),
            event,
        }));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Update> {
        self.updates.subscribe()
    }

    pub fn snapshots(&self) -> Vec<Snapshot> {
        self.snapshots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn snapshot(&self, account: &str) -> Option<Snapshot> {
        self.snapshots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .cloned()
    }

    /// Attaches the stored history the dashboard reads from.
    pub fn set_history(&self, history: Arc<dyn History>) {
        *self.history.write().unwrap_or_else(|e| e.into_inner()) = Some(history);
    }

    pub fn history(&self, account: &str, limit: u32) -> Vec<HistoryRow> {
        let guard = self.history.read().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .map(|h| h.recent(account, limit))
            .unwrap_or_default()
    }

    pub fn rewards(&self, account: &str, limit: u32) -> Vec<RewardRow> {
        let guard = self.history.read().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .map(|h| h.rewards(account, limit))
            .unwrap_or_default()
    }

    pub fn set_enrolment(&self, enrolment: Arc<dyn Enrolment>) {
        *self.enrolment.write().unwrap_or_else(|e| e.into_inner()) = Some(enrolment);
    }

    /// Starts a device-code login for an account.
    pub fn begin_login(&self, account: &str) -> Result<(), String> {
        let enrolment = {
            let guard = self.enrolment.read().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        match enrolment {
            Some(e) => e.begin(account),
            None => Err("this miner cannot log in from the dashboard".into()),
        }
    }

    pub fn set_login_state(&self, state: LoginState) {
        self.logins
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(state.account.clone(), state);
    }

    pub fn login_states(&self) -> Vec<LoginState> {
        self.logins
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Records why an account's miner stopped.
    pub fn set_account_error(&self, account: &str, detail: &str) {
        self.errors
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_owned(), detail.to_owned());
    }

    pub fn clear_account_error(&self, account: &str) {
        self.errors
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
    }

    pub fn account_errors(&self) -> BTreeMap<String, String> {
        self.errors
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Registers an account's command channel. Called by the supervisor as it
    /// starts each miner.
    pub fn register(&self, account: &str, tx: mpsc::Sender<Command>) {
        self.commands
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_owned(), tx);
    }

    pub fn unregister(&self, account: &str) {
        self.commands
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
    }

    pub fn accounts(&self) -> Vec<String> {
        self.commands
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Sends a command to one account's miner.
    ///
    /// `try_send` rather than `send`: an HTTP handler must not block on a
    /// miner that is mid-request, and a full queue means the user is clicking
    /// faster than the miner can act, which is worth reporting rather than
    /// silently absorbing.
    pub fn send_command(&self, account: &str, command: Command) -> Result<(), CommandError> {
        let tx = {
            let guard = self.commands.read().unwrap_or_else(|e| e.into_inner());
            guard
                .get(account)
                .cloned()
                .ok_or_else(|| CommandError::UnknownAccount(account.to_owned()))?
        };
        tx.try_send(command)
            .map_err(|_| CommandError::NotAccepting(account.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btdm_core::event::EventKind;
    use btdm_core::model::ChannelId;
    use btdm_core::snapshot::{HealthView, SettingsView, Totals};

    fn snapshot(account: &str) -> Snapshot {
        Snapshot {
            account: account.into(),
            login: Some("someone".into()),
            strategy: "deadline".into(),
            state: "Mining".into(),
            paused: false,
            generated_at: chrono::Utc::now(),
            watching: None,
            eligible: vec![],
            rejected: vec![],
            live: vec![],
            settings: SettingsView::default(),
            strategies: vec![],
            health: HealthView::default(),
            totals: Totals::default(),
        }
    }

    #[test]
    fn a_published_snapshot_can_be_read_back() {
        let hub = Hub::new();
        hub.publish(snapshot("main"));
        assert_eq!(hub.snapshot("main").unwrap().account, "main");
        assert!(hub.snapshot("absent").is_none());
    }

    #[test]
    fn republishing_replaces_rather_than_duplicates() {
        let hub = Hub::new();
        hub.publish(snapshot("main"));
        let mut updated = snapshot("main");
        updated.state = "Idle".into();
        hub.publish(updated);

        assert_eq!(hub.snapshots().len(), 1);
        assert_eq!(hub.snapshot("main").unwrap().state, "Idle");
    }

    #[test]
    fn accounts_are_listed_in_a_stable_order() {
        let hub = Hub::new();
        hub.publish(snapshot("zeta"));
        hub.publish(snapshot("alpha"));
        let names: Vec<_> = hub.snapshots().into_iter().map(|s| s.account).collect();
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    #[tokio::test]
    async fn subscribers_receive_tagged_events() {
        let hub = Hub::new();
        let mut rx = hub.subscribe();

        hub.emit(
            "main",
            MinerEvent {
                at: chrono::Utc::now(),
                kind: EventKind::WatchSent {
                    channel: ChannelId::from("c1"),
                },
            },
        );

        match rx.recv().await.unwrap() {
            Update::Event(e) => assert_eq!(e.account, "main"),
            other => panic!("expected an event, got {other:?}"),
        }
    }

    /// The page renders from pushed snapshots, so publishing has to reach
    /// subscribers rather than only updating the map.
    #[tokio::test]
    async fn publishing_pushes_the_snapshot_to_subscribers() {
        let hub = Hub::new();
        let mut rx = hub.subscribe();

        hub.publish(snapshot("main"));

        match rx.recv().await.unwrap() {
            Update::Snapshot(s) => assert_eq!(s.account, "main"),
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    /// Nobody having the dashboard open must not break the miner.
    #[test]
    fn emitting_with_no_subscribers_is_harmless() {
        let hub = Hub::new();
        hub.emit(
            "main",
            MinerEvent {
                at: chrono::Utc::now(),
                kind: EventKind::AuthExpired,
            },
        );
        hub.publish(snapshot("main"));
    }

    #[tokio::test]
    async fn a_registered_account_receives_its_commands() {
        let hub = Hub::new();
        let (tx, mut rx) = mpsc::channel(4);
        hub.register("main", tx);

        hub.send_command("main", Command::Pause).unwrap();
        assert_eq!(rx.recv().await.unwrap(), Command::Pause);
    }

    #[test]
    fn commanding_an_unknown_account_says_so() {
        let hub = Hub::new();
        assert_eq!(
            hub.send_command("nope", Command::Pause),
            Err(CommandError::UnknownAccount("nope".into()))
        );
    }

    /// A miner that has stopped must not leave the dashboard reporting success
    /// for commands nobody will ever act on.
    #[tokio::test]
    async fn commanding_a_stopped_miner_fails_loudly() {
        let hub = Hub::new();
        let (tx, rx) = mpsc::channel(1);
        hub.register("main", tx);
        drop(rx);

        assert_eq!(
            hub.send_command("main", Command::Pause),
            Err(CommandError::NotAccepting("main".into()))
        );
    }

    #[test]
    fn unregistering_removes_the_account() {
        let hub = Hub::new();
        let (tx, _rx) = mpsc::channel(1);
        hub.register("main", tx);
        assert_eq!(hub.accounts(), vec!["main".to_string()]);
        hub.unregister("main");
        assert!(hub.accounts().is_empty());
    }
}
