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

use btdm_core::control::{Command, CommandEffect};
use serde::Serialize;
use btdm_core::event::MinerEvent;
use btdm_core::snapshot::Snapshot;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc, oneshot};

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
    /// Queued, but the miner had not acted on it before we stopped waiting.
    /// The command is not lost — only its outcome is unknown.
    #[error("the miner for '{0}' took the command but has not acted on it yet")]
    Slow(String),
}

/// How long to wait for a miner to apply a command before answering without
/// knowing the outcome. Generous enough to cover a selection pass, short
/// enough that a wedged miner does not freeze the page.
pub const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A command and somewhere to put the answer.
///
/// The reply channel is what lets a caller distinguish "the miner accepted
/// this" from "the miner did this", which are different things when the queue
/// is deep or the miner is mid-request.
#[derive(Debug)]
pub struct Envelope {
    pub command: Command,
    pub reply: oneshot::Sender<CommandEffect>,
}

#[derive(Clone)]
pub struct Hub {
    /// Ordered so the dashboard lists accounts consistently between refreshes.
    snapshots: Arc<RwLock<BTreeMap<String, Snapshot>>>,
    commands: Arc<RwLock<BTreeMap<String, mpsc::Sender<Envelope>>>>,
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
    pub fn register(&self, account: &str, tx: mpsc::Sender<Envelope>) {
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

    /// Sends a command to one account's miner and waits for it to be applied.
    ///
    /// `try_send` rather than `send`: an HTTP handler must not block on a
    /// miner that is mid-request, and a full queue means the user is clicking
    /// faster than the miner can act, which is worth reporting rather than
    /// silently absorbing.
    ///
    /// The *reply* is awaited, though, so callers learn what the command
    /// actually did. Without it the only honest thing a caller could report
    /// was "sent" — and anything that read the published snapshot back to find
    /// out was racing the miner's own publish schedule, which is how two quick
    /// edits to a list used to lose one of them.
    pub async fn send_command(
        &self,
        account: &str,
        command: Command,
    ) -> Result<CommandEffect, CommandError> {
        let tx = {
            let guard = self.commands.read().unwrap_or_else(|e| e.into_inner());
            guard
                .get(account)
                .cloned()
                .ok_or_else(|| CommandError::UnknownAccount(account.to_owned()))?
        };
        let (reply, answer) = oneshot::channel();
        tx.try_send(Envelope { command, reply })
            .map_err(|_| CommandError::NotAccepting(account.to_owned()))?;

        // Bounded. A miner wedged mid-request would otherwise hold every
        // dashboard click open forever, which is a worse failure than not
        // knowing the outcome: the page would simply stop responding.
        //
        // Normally this resolves in milliseconds — the command arm sits in the
        // same `select!` as the heartbeat wait, so it is picked up as soon as
        // it arrives. The budget only matters while a selection pass is in
        // flight, which is the one thing that can take seconds.
        match tokio::time::timeout(COMMAND_TIMEOUT, answer).await {
            Ok(Ok(effect)) => Ok(effect),
            // The miner took the command and then dropped the reply: it is
            // gone, and nothing will act on it.
            Ok(Err(_)) => Err(CommandError::NotAccepting(account.to_owned())),
            Err(_) => Err(CommandError::Slow(account.to_owned())),
        }
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

        // The miner half: take the command and answer it.
        let miner = tokio::spawn(async move {
            let envelope = rx.recv().await.expect("a command arrives");
            assert_eq!(envelope.command, Command::Pause);
            let _ = envelope.reply.send(CommandEffect::message("Paused"));
        });

        let effect = hub.send_command("main", Command::Pause).await.unwrap();
        assert_eq!(effect.message, "Paused");
        miner.await.unwrap();
    }

    /// The caller must learn what the command actually did, not merely that it
    /// was queued — anything that instead read the published snapshot back was
    /// racing the miner's own publish schedule.
    #[tokio::test]
    async fn the_reply_carries_the_miners_own_answer() {
        let hub = Hub::new();
        let (tx, mut rx) = mpsc::channel(4);
        hub.register("main", tx);

        tokio::spawn(async move {
            let envelope = rx.recv().await.unwrap();
            let _ = envelope
                .reply
                .send(CommandEffect::message("Prioritised 'rust' at position 2"));
        });

        let effect = hub
            .send_command("main", Command::AddPriorityEntry { entry: "rust".into() })
            .await
            .unwrap();
        assert_eq!(effect.message, "Prioritised 'rust' at position 2");
    }

    #[tokio::test]
    async fn commanding_an_unknown_account_says_so() {
        let hub = Hub::new();
        assert_eq!(
            hub.send_command("nope", Command::Pause).await,
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
            hub.send_command("main", Command::Pause).await,
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
