//! Runtime commands.
//!
//! Everything the dashboard can ask the miner to do is one of these. They are
//! pure data so the engine can apply them without knowing whether they arrived
//! over HTTP, from a config reload, or from a test.
//!
//! The rule is that a command takes effect *now*. Before this existed, every
//! setting lived in `config.toml` and needed a restart, and a change to the
//! strategy was invisible until the next 30-minute resync.

use crate::model::CampaignId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Stop watching and stop selecting. Campaign syncing continues, so the
    /// dashboard stays truthful about what is expiring while paused.
    Pause,
    Resume,
    SetStrategy { name: String },
    /// `None` clears the pin.
    Pin { channel: Option<String> },
    SetFavourite { login: String, on: bool },
    /// Replace the priority list outright.
    SetPriorityGames { games: Vec<String> },
    /// Append one entry, if it is not already listed.
    ///
    /// Editing the list one entry at a time has to happen inside the engine.
    /// A caller that reads the list, appends to it and sends the result is
    /// racing the snapshot it read from: the dashboard republishes on its own
    /// schedule, so two quick edits both start from the same stale list and
    /// the first is silently undone by the second.
    AddPriorityEntry { entry: String },
    RemovePriorityEntry { entry: String },
    /// Move an entry one place towards the front, or towards the back.
    MovePriorityEntry { entry: String, up: bool },
    SetCampaignExcluded { campaign: CampaignId, excluded: bool },
    SetGameExcluded { game: String, excluded: bool },
    /// Leave the current channel and choose again immediately.
    SwitchChannel,
    /// Watch a specific channel on the next selection, once, without changing
    /// the strategy. This is the dashboard's "watch this one" button.
    WatchChannel { login: String },
    /// Re-fetch campaigns now rather than at the next scheduled sync.
    SyncNow,
    /// Forget every channel and campaign penalty.
    ClearQuarantine,
}

/// What applying a command requires of the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandEffect {
    /// Campaigns should be re-fetched before the next selection.
    pub resync: bool,
    /// Selection should run again immediately.
    pub reselect: bool,
    /// A persisted setting changed and should be written back to config.
    pub persist: bool,
    /// Human-readable confirmation, echoed to the dashboard and the log.
    pub message: String,
}

impl CommandEffect {
    pub fn message(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            ..Default::default()
        }
    }

    pub fn with_resync(mut self) -> Self {
        self.resync = true;
        self
    }

    pub fn with_reselect(mut self) -> Self {
        self.reselect = true;
        self
    }

    pub fn with_persist(mut self) -> Self {
        self.persist = true;
        self
    }
}

/// Settings a command may change, so the caller can write them back without
/// reaching into the engine's internals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSettings {
    pub strategy: String,
    pub priority_games: Vec<String>,
    pub favourite_channels: Vec<String>,
    pub pinned_channel: Option<String>,
    pub excluded_campaigns: Vec<String>,
    pub excluded_games: Vec<String>,
    pub paused: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dashboard posts these as JSON, so the wire shape is part of the
    /// contract rather than an implementation detail.
    #[test]
    fn commands_round_trip_as_tagged_json() {
        let cases = vec![
            Command::Pause,
            Command::SetStrategy {
                name: "coverage".into(),
            },
            Command::Pin {
                channel: Some("someone".into()),
            },
            Command::Pin { channel: None },
            Command::SetFavourite {
                login: "someone".into(),
                on: true,
            },
            Command::SetCampaignExcluded {
                campaign: CampaignId::from("c1"),
                excluded: true,
            },
            Command::WatchChannel {
                login: "someone".into(),
            },
        ];
        for c in cases {
            let json = serde_json::to_string(&c).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(back, c, "round trip failed for {json}");
        }
    }

    #[test]
    fn the_tag_is_a_readable_snake_case_name() {
        let json = serde_json::to_string(&Command::SyncNow).unwrap();
        assert!(json.contains("\"sync_now\""), "got {json}");
    }

    #[test]
    fn effects_compose() {
        let e = CommandEffect::message("done").with_resync().with_reselect();
        assert!(e.resync && e.reselect && !e.persist);
        assert_eq!(e.message, "done");
    }
}
