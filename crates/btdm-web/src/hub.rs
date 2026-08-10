//! Shared state between the miners and the dashboard.
//!
//! Each account's task publishes a snapshot and broadcasts events; the web
//! layer only ever reads. Accounts never see each other's state, which is what
//! keeps them independent.

use btdm_core::event::MinerEvent;
use btdm_core::snapshot::Snapshot;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

/// An event tagged with the account that produced it.
#[derive(Debug, Clone)]
pub struct AccountEvent {
    pub account: String,
    pub event: MinerEvent,
}

#[derive(Clone)]
pub struct Hub {
    /// Ordered so the dashboard lists accounts consistently between refreshes.
    snapshots: Arc<RwLock<BTreeMap<String, Snapshot>>>,
    events: broadcast::Sender<AccountEvent>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            snapshots: Arc::new(RwLock::new(BTreeMap::new())),
            events,
        }
    }

    pub fn publish(&self, snapshot: Snapshot) {
        let mut guard = self
            .snapshots
            .write()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(snapshot.account.clone(), snapshot);
    }

    /// Broadcasting with no subscribers is not an error — nobody has the
    /// dashboard open.
    pub fn emit(&self, account: &str, event: MinerEvent) {
        let _ = self.events.send(AccountEvent {
            account: account.to_owned(),
            event,
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AccountEvent> {
        self.events.subscribe()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use btdm_core::event::EventKind;
    use btdm_core::model::ChannelId;
    use btdm_core::snapshot::Totals;

    fn snapshot(account: &str) -> Snapshot {
        Snapshot {
            account: account.into(),
            login: Some("someone".into()),
            strategy: "deadline".into(),
            state: "Mining".into(),
            generated_at: chrono::Utc::now(),
            watching: None,
            eligible: vec![],
            rejected: vec![],
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

        let got = rx.recv().await.unwrap();
        assert_eq!(got.account, "main");
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
    }
}
