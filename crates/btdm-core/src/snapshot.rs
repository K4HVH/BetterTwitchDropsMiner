//! A serialisable view of what the miner is doing, for the dashboard.
//!
//! Every figure here is server-confirmed or explicitly labelled as unknown.
//! There is deliberately no field that a local timer could fill in, because a
//! dashboard that can display invented progress is the problem this project
//! exists to solve.

use crate::model::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub account: String,
    pub login: Option<String>,
    pub strategy: String,
    pub state: String,
    pub generated_at: DateTime<Utc>,
    pub watching: Option<WatchingView>,
    /// Campaigns that can be mined, soonest deadline first.
    pub eligible: Vec<CampaignView>,
    /// Campaigns that cannot be mined, each with the reason.
    pub rejected: Vec<RejectedView>,
    pub totals: Totals,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchingView {
    pub channel_login: String,
    pub channel_display_name: String,
    pub campaign: String,
    /// The drop actually advancing, if Twitch has confirmed one.
    pub drop_name: Option<String>,
    pub current_minutes: Option<u32>,
    pub required_minutes: Option<u32>,
    /// Seconds since Twitch last confirmed progress. Climbing means trouble.
    pub seconds_since_confirmation: Option<i64>,
    /// True once the stall threshold has passed without a confirmation.
    pub stalled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignView {
    pub id: String,
    pub name: String,
    pub game: String,
    pub ends_at: DateTime<Utc>,
    /// Minutes of watching still needed to finish every reachable tier.
    pub remaining_minutes: u32,
    /// Spare time before the deadline. Negative means already impossible.
    pub slack_minutes: i64,
    /// Slack is thin enough that this campaign could be lost.
    pub at_risk: bool,
    pub drops: Vec<DropView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropView {
    pub id: String,
    pub name: String,
    pub current_minutes: u32,
    pub required_minutes: u32,
    pub is_claimed: bool,
    pub claimable: bool,
    pub benefits: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedView {
    pub id: String,
    pub name: String,
    pub game: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub campaigns_known: usize,
    pub campaigns_eligible: usize,
    pub drops_claimed: usize,
    pub drops_remaining: usize,
}

/// A campaign with less than this much slack is flagged as at risk.
pub const AT_RISK_SLACK_MINUTES: i64 = 120;

pub fn drop_view(d: &TimedDrop) -> DropView {
    DropView {
        id: d.id.0.clone(),
        name: d.name.clone(),
        current_minutes: d.current_minutes,
        required_minutes: d.required_minutes,
        is_claimed: d.is_claimed,
        claimable: d.is_claimable(),
        benefits: d.benefits.iter().map(|b| b.name.clone()).collect(),
    }
}

pub fn campaign_view(c: &DropCampaign, now: DateTime<Utc>) -> CampaignView {
    let remaining = crate::eligibility::remaining_minutes(c);
    let slack = crate::strategy::slack(c, now).num_minutes();
    CampaignView {
        id: c.id.0.clone(),
        name: c.name.clone(),
        game: c.game.name.clone(),
        ends_at: c.ends_at,
        remaining_minutes: remaining,
        slack_minutes: slack,
        at_risk: slack < AT_RISK_SLACK_MINUTES,
        drops: c.drops.iter().map(drop_view).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap()
    }

    fn campaign(ends_in: Duration, required: u32, current: u32) -> DropCampaign {
        DropCampaign {
            id: CampaignId::from("c1"),
            name: "Camp".into(),
            game: Game {
                id: GameId::from("g1"),
                name: "Deadlock".into(),
                slug: "deadlock".into(),
            },
            starts_at: now() - Duration::days(1),
            ends_at: now() + ends_in,
            is_account_linked: true,
            allowed_channels: vec![],
            drops: vec![TimedDrop {
                id: DropId::from("d1"),
                name: "Reward".into(),
                benefits: vec![Benefit {
                    id: "b1".into(),
                    name: "Skin".into(),
                    kind: BenefitKind::Entitlement,
                }],
                required_minutes: required,
                current_minutes: current,
                claim_id: None,
                is_claimed: false,
                precondition_ids: vec![],
            }],
        }
    }

    #[test]
    fn a_comfortable_campaign_is_not_at_risk() {
        let v = campaign_view(&campaign(Duration::days(10), 60, 0), now());
        assert_eq!(v.remaining_minutes, 60);
        assert!(v.slack_minutes > 0);
        assert!(!v.at_risk);
    }

    #[test]
    fn a_tight_campaign_is_flagged_at_risk() {
        // Needs 60 minutes, only 90 minutes left.
        let v = campaign_view(&campaign(Duration::minutes(90), 60, 0), now());
        assert_eq!(v.slack_minutes, 30);
        assert!(v.at_risk);
    }

    #[test]
    fn an_impossible_campaign_has_negative_slack() {
        // Needs 600 minutes, one hour left.
        let v = campaign_view(&campaign(Duration::hours(1), 600, 0), now());
        assert!(v.slack_minutes < 0);
        assert!(v.at_risk);
    }

    #[test]
    fn partial_progress_reduces_the_remaining_time() {
        let v = campaign_view(&campaign(Duration::days(10), 60, 23), now());
        assert_eq!(v.remaining_minutes, 37);
    }

    #[test]
    fn drop_view_carries_benefit_names() {
        let v = campaign_view(&campaign(Duration::days(10), 60, 0), now());
        assert_eq!(v.drops[0].benefits, vec!["Skin".to_string()]);
        assert!(!v.drops[0].claimable);
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let snap = Snapshot {
            account: "default".into(),
            login: Some("k4hvh".into()),
            strategy: "deadline".into(),
            state: "Mining".into(),
            generated_at: now(),
            watching: Some(WatchingView {
                channel_login: "someone".into(),
                channel_display_name: "Someone".into(),
                campaign: "Camp".into(),
                drop_name: Some("Reward".into()),
                current_minutes: Some(23),
                required_minutes: Some(60),
                seconds_since_confirmation: Some(41),
                stalled: false,
            }),
            eligible: vec![campaign_view(&campaign(Duration::days(1), 60, 0), now())],
            rejected: vec![],
            totals: Totals::default(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.login.as_deref(), Some("k4hvh"));
        assert_eq!(back.watching.unwrap().current_minutes, Some(23));
    }
}
