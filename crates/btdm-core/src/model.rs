//! The domain vocabulary. Pure data — no I/O, no Twitch JSON shapes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_owned()) }
        }
    )*};
}

id_type!(ChannelId, GameId, CampaignId, DropId, UserId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Game {
    pub id: GameId,
    pub name: String,
    /// URL slug, needed to query the game's channel directory.
    pub slug: String,
    /// Box art, when Twitch supplied one. Display only.
    #[serde(default)]
    pub box_art_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    /// Lowercase login name, used in URLs and GQL variables.
    pub login: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    /// Changes when a stream restarts. A stale value earns nothing while
    /// continuing to look healthy, which is a known cause of silent stalls.
    pub broadcast_id: String,
    pub game: Option<Game>,
    pub title: String,
    pub viewers: u32,
    /// Whether Twitch reports drops as enabled for this stream.
    pub drops_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BenefitKind {
    Badge,
    Emote,
    /// Anything else, including in-game items.
    Entitlement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Benefit {
    pub id: String,
    pub name: String,
    pub kind: BenefitKind,
    /// Artwork for the reward. Display only — a missing image never affects
    /// whether a drop is mineable.
    #[serde(default)]
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedDrop {
    pub id: DropId,
    pub name: String,
    pub benefits: Vec<Benefit>,
    pub required_minutes: u32,
    /// Server-confirmed minutes. Never advanced by a local timer.
    pub current_minutes: u32,
    /// Present once the drop is earned and awaiting a claim.
    pub claim_id: Option<String>,
    pub is_claimed: bool,
    /// Drops that must be claimed before this tier unlocks.
    pub precondition_ids: Vec<DropId>,
}

impl TimedDrop {
    pub fn remaining_minutes(&self) -> u32 {
        self.required_minutes.saturating_sub(self.current_minutes)
    }

    pub fn is_claimable(&self) -> bool {
        !self.is_claimed && self.claim_id.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropCampaign {
    pub id: CampaignId,
    pub name: String,
    pub game: Game,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub drops: Vec<TimedDrop>,
    /// Whether Twitch reports this account as linked to the campaign.
    /// Never inferred from local settings.
    pub is_account_linked: bool,
    /// Channel allow-list. Empty means any channel streaming the game qualifies.
    pub allowed_channels: Vec<Channel>,
}

impl DropCampaign {
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        now >= self.starts_at && now < self.ends_at
    }

    pub fn has_badge_or_emote(&self) -> bool {
        self.drops
            .iter()
            .flat_map(|d| &d.benefits)
            .any(|b| matches!(b.kind, BenefitKind::Badge | BenefitKind::Emote))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, h, 0, 0).unwrap()
    }

    fn drop_with(required: u32, current: u32) -> TimedDrop {
        TimedDrop {
            id: DropId("d1".into()),
            name: "Test Drop".into(),
            benefits: vec![],
            required_minutes: required,
            current_minutes: current,
            claim_id: None,
            is_claimed: false,
            precondition_ids: vec![],
        }
    }

    fn campaign(drops: Vec<TimedDrop>) -> DropCampaign {
        DropCampaign {
            id: CampaignId("c1".into()),
            name: "Camp".into(),
            game: Game {
                id: GameId("g1".into()),
                name: "Game".into(),
                slug: "game".into(),
                box_art_url: None,
            },
            starts_at: at(10),
            ends_at: at(20),
            drops,
            is_account_linked: true,
            allowed_channels: vec![],
        }
    }

    #[test]
    fn remaining_minutes_counts_down() {
        assert_eq!(drop_with(60, 23).remaining_minutes(), 37);
    }

    #[test]
    fn remaining_minutes_saturates_at_zero() {
        // Twitch occasionally reports more minutes than required.
        assert_eq!(drop_with(60, 75).remaining_minutes(), 0);
    }

    #[test]
    fn drop_is_claimable_only_with_a_claim_id_and_not_already_claimed() {
        let mut d = drop_with(60, 60);
        assert!(!d.is_claimable(), "no claim id yet");

        d.claim_id = Some("claim-1".into());
        assert!(d.is_claimable());

        d.is_claimed = true;
        assert!(!d.is_claimable(), "already claimed");
    }

    #[test]
    fn campaign_is_active_only_inside_its_window() {
        let c = campaign(vec![]);
        assert!(!c.is_active(at(9)));
        assert!(c.is_active(at(15)));
        assert!(!c.is_active(at(21)));
    }

    #[test]
    fn campaign_detects_badge_and_emote_benefits() {
        let mut c = campaign(vec![drop_with(60, 0)]);
        assert!(!c.has_badge_or_emote());

        c.drops[0].benefits.push(Benefit {
            id: "b1".into(),
            name: "Shiny Badge".into(),
            kind: BenefitKind::Badge,
            image_url: None,
        });
        assert!(c.has_badge_or_emote());
    }
}
