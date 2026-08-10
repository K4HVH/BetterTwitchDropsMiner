//! A serialisable view of what the miner is doing, for the dashboard.
//!
//! Every figure here is server-confirmed or explicitly labelled as unknown.
//! There is deliberately no field that a local timer could fill in, because a
//! dashboard that can display invented progress is the problem this project
//! exists to solve.
//!
//! The snapshot is the *whole* UI contract: the page renders this and nothing
//! else, so anything the user should be able to see or change has to appear
//! here. That includes current settings and the list of strategies, so the
//! controls can be built from the payload rather than hard-coded.

use crate::model::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub account: String,
    pub login: Option<String>,
    pub strategy: String,
    pub state: String,
    /// Mining is suspended by the user. Syncing continues, so deadlines stay
    /// truthful while paused.
    pub paused: bool,
    pub generated_at: DateTime<Utc>,
    pub watching: Option<WatchingView>,
    /// Campaigns that can be mined, soonest deadline first.
    pub eligible: Vec<CampaignView>,
    /// Campaigns that cannot be mined, each with the reason.
    pub rejected: Vec<RejectedView>,
    /// Channels seen live in the last selection pass, best first.
    pub live: Vec<LiveChannelView>,
    pub settings: SettingsView,
    pub strategies: Vec<StrategyView>,
    pub health: HealthView,
    pub totals: Totals,
}

/// The user's current settings, so the dashboard can show controls in their
/// real positions rather than guessing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SettingsView {
    pub strategy: String,
    pub priority_games: Vec<String>,
    pub favourites: Vec<String>,
    pub pinned: Option<String>,
    pub excluded_campaigns: Vec<String>,
    pub excluded_games: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyView {
    pub name: String,
    pub description: String,
    pub selected: bool,
}

/// Everything about whether mining is actually working.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthView {
    /// The watchdog's current verdict.
    pub verdict: String,
    pub seconds_since_confirmation: Option<i64>,
    /// The recovery rung being worked, when one is.
    pub recovery_step: Option<String>,
    pub last_sync: Option<DateTime<Utc>>,
    pub next_sync_in_seconds: Option<i64>,
    pub quarantined_channels: Vec<QuarantineView>,
    pub quarantined_campaigns: Vec<QuarantineView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineView {
    pub name: String,
    pub until: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchingView {
    pub channel_login: String,
    pub channel_display_name: String,
    pub stream_title: String,
    pub viewers: u32,
    /// The campaign whose deadline drove the choice.
    pub campaign: String,
    /// Every campaign this one channel is advancing at once. This is the whole
    /// point of the channel-centric candidate model, so it is shown.
    pub campaigns: Vec<String>,
    /// Why the strategy picked this channel, in words.
    pub reason: String,
    pub since: DateTime<Utc>,
    /// The drop actually advancing, if Twitch has confirmed one.
    pub drop_name: Option<String>,
    pub current_minutes: Option<u32>,
    pub required_minutes: Option<u32>,
    /// Seconds since Twitch last confirmed progress. Climbing means trouble.
    pub seconds_since_confirmation: Option<i64>,
    /// True once the stall threshold has passed without a confirmation.
    pub stalled: bool,
}

/// A channel that could be watched right now, so the user can see the actual
/// choice the strategy faced — and override it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveChannelView {
    pub login: String,
    pub display_name: String,
    pub viewers: u32,
    pub title: String,
    pub game: Option<String>,
    /// Names of every campaign this channel would advance at once.
    pub campaigns: Vec<String>,
    pub drops: usize,
    pub is_current: bool,
    pub is_favourite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignView {
    pub id: String,
    pub name: String,
    pub game: String,
    pub game_id: String,
    pub box_art_url: Option<String>,
    pub account_link_url: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    /// Minutes of watching still needed to finish every reachable tier.
    pub remaining_minutes: u32,
    /// Spare time before the deadline. Negative means already impossible.
    pub slack_minutes: i64,
    /// Slack is thin enough that this campaign could be lost.
    pub at_risk: bool,
    /// The user excluded this campaign, so it is never selected.
    pub excluded: bool,
    /// This campaign is being advanced by the channel being watched.
    pub is_current: bool,
    /// Channels seen live for it in the last pass.
    pub live_channels: usize,
    /// Non-empty when Twitch restricts the campaign to specific channels.
    pub allowed_channels: Vec<String>,
    pub drops: Vec<DropView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropView {
    pub id: String,
    pub name: String,
    pub current_minutes: u32,
    pub required_minutes: u32,
    pub percent: f64,
    pub is_claimed: bool,
    pub claimable: bool,
    /// Earlier tiers must be claimed before this one unlocks.
    pub locked: bool,
    /// This is the drop Twitch is currently advancing.
    pub active: bool,
    pub benefits: Vec<BenefitView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenefitView {
    pub name: String,
    pub kind: String,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedView {
    pub id: String,
    pub name: String,
    pub game: String,
    pub reason: String,
    /// A plain-English explanation. `NotLinked` means nothing to a user who
    /// does not know it refers to Twitch account linking.
    pub explanation: String,
    /// Where to go and fix it, when there is somewhere to go.
    pub action_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub campaigns_known: usize,
    pub campaigns_eligible: usize,
    pub campaigns_at_risk: usize,
    pub drops_claimed: usize,
    pub drops_remaining: usize,
    pub drops_claimable: usize,
    /// Total watching still needed across every mineable campaign.
    pub minutes_remaining: u32,
}

/// A campaign with less than this much slack is flagged as at risk.
pub const AT_RISK_SLACK_MINUTES: i64 = 120;

/// Context the campaign views need that a campaign does not carry itself.
pub struct ViewContext<'a> {
    pub now: DateTime<Utc>,
    pub excluded_campaigns: &'a HashSet<String>,
    pub excluded_games: &'a HashSet<String>,
    /// Campaigns the watched channel is advancing.
    pub current_campaigns: &'a [CampaignId],
    /// The one drop Twitch is confirming progress for.
    pub active_drop: Option<&'a DropId>,
    /// Live channel counts, keyed by campaign id.
    pub live_by_campaign: &'a HashMap<String, usize>,
}

impl ViewContext<'_> {
    pub fn is_excluded(&self, c: &DropCampaign) -> bool {
        self.excluded_campaigns.contains(&c.id.0)
            || self.excluded_games.contains(&c.game.name.to_lowercase())
    }
}

pub fn benefit_view(b: &Benefit) -> BenefitView {
    BenefitView {
        name: b.name.clone(),
        kind: format!("{:?}", b.kind),
        image_url: b.image_url.clone(),
    }
}

pub fn drop_view(d: &TimedDrop, claimed: &HashSet<DropId>, active: Option<&DropId>) -> DropView {
    // A tier is locked until every tier it depends on has been claimed.
    let locked = d
        .precondition_ids
        .iter()
        .any(|p| !claimed.contains(p));

    DropView {
        id: d.id.0.clone(),
        name: d.name.clone(),
        current_minutes: d.current_minutes,
        required_minutes: d.required_minutes,
        percent: if d.required_minutes == 0 {
            0.0
        } else {
            (d.current_minutes as f64 / d.required_minutes as f64 * 100.0).min(100.0)
        },
        is_claimed: d.is_claimed,
        claimable: d.is_claimable(),
        locked,
        active: active == Some(&d.id),
        benefits: d.benefits.iter().map(benefit_view).collect(),
    }
}

pub fn campaign_view(c: &DropCampaign, ctx: &ViewContext) -> CampaignView {
    let remaining = crate::eligibility::remaining_minutes(c);
    let slack = crate::strategy::slack(c, ctx.now).num_minutes();
    let claimed: HashSet<DropId> = c
        .drops
        .iter()
        .filter(|d| d.is_claimed)
        .map(|d| d.id.clone())
        .collect();

    CampaignView {
        id: c.id.0.clone(),
        name: c.name.clone(),
        game: c.game.name.clone(),
        game_id: c.game.id.0.clone(),
        box_art_url: c.game.box_art_url.clone(),
        account_link_url: c.account_link_url.clone(),
        starts_at: c.starts_at,
        ends_at: c.ends_at,
        remaining_minutes: remaining,
        slack_minutes: slack,
        at_risk: slack < AT_RISK_SLACK_MINUTES,
        excluded: ctx.is_excluded(c),
        is_current: ctx.current_campaigns.contains(&c.id),
        live_channels: ctx.live_by_campaign.get(&c.id.0).copied().unwrap_or(0),
        allowed_channels: c
            .allowed_channels
            .iter()
            .map(|ch| ch.display_name.clone())
            .collect(),
        drops: c
            .drops
            .iter()
            .map(|d| drop_view(d, &claimed, ctx.active_drop))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap()
    }

    fn ctx<'a>(
        excluded: &'a HashSet<String>,
        games: &'a HashSet<String>,
        current: &'a [CampaignId],
        live: &'a HashMap<String, usize>,
    ) -> ViewContext<'a> {
        ViewContext {
            now: now(),
            excluded_campaigns: excluded,
            excluded_games: games,
            current_campaigns: current,
            active_drop: None,
            live_by_campaign: live,
        }
    }

    fn empty_ctx() -> (HashSet<String>, HashSet<String>, Vec<CampaignId>, HashMap<String, usize>)
    {
        (HashSet::new(), HashSet::new(), Vec::new(), HashMap::new())
    }

    fn campaign(ends_in: Duration, required: u32, current: u32) -> DropCampaign {
        DropCampaign {
            id: CampaignId::from("c1"),
            name: "Camp".into(),
            game: Game {
                id: GameId::from("g1"),
                name: "Deadlock".into(),
                slug: "deadlock".into(),
                box_art_url: None,
            },
            starts_at: now() - Duration::days(1),
            ends_at: now() + ends_in,
            is_account_linked: true,
            allowed_channels: vec![],
            account_link_url: None,
            drops: vec![TimedDrop {
                id: DropId::from("d1"),
                name: "Reward".into(),
                benefits: vec![Benefit {
                    id: "b1".into(),
                    name: "Skin".into(),
                    kind: BenefitKind::Entitlement,
                    image_url: Some("https://example.invalid/skin.png".into()),
                }],
                required_minutes: required,
                current_minutes: current,
                claim_id: None,
                is_claimed: false,
                precondition_ids: vec![],
            }],
        }
    }

    fn view(c: &DropCampaign) -> CampaignView {
        let (a, b, cc, d) = empty_ctx();
        campaign_view(c, &ctx(&a, &b, &cc, &d))
    }

    #[test]
    fn a_comfortable_campaign_is_not_at_risk() {
        let v = view(&campaign(Duration::days(10), 60, 0));
        assert_eq!(v.remaining_minutes, 60);
        assert!(v.slack_minutes > 0);
        assert!(!v.at_risk);
    }

    #[test]
    fn a_tight_campaign_is_flagged_at_risk() {
        // Needs 60 minutes, only 90 minutes left.
        let v = view(&campaign(Duration::minutes(90), 60, 0));
        assert_eq!(v.slack_minutes, 30);
        assert!(v.at_risk);
    }

    #[test]
    fn an_impossible_campaign_has_negative_slack() {
        // Needs 600 minutes, one hour left.
        let v = view(&campaign(Duration::hours(1), 600, 0));
        assert!(v.slack_minutes < 0);
        assert!(v.at_risk);
    }

    #[test]
    fn partial_progress_reduces_the_remaining_time() {
        let v = view(&campaign(Duration::days(10), 60, 23));
        assert_eq!(v.remaining_minutes, 37);
    }

    #[test]
    fn drop_view_carries_benefits_and_artwork() {
        let v = view(&campaign(Duration::days(10), 60, 0));
        assert_eq!(v.drops[0].benefits[0].name, "Skin");
        assert!(v.drops[0].benefits[0].image_url.is_some());
        assert!(!v.drops[0].claimable);
    }

    #[test]
    fn percent_is_reported_for_the_progress_bar() {
        let v = view(&campaign(Duration::days(10), 60, 15));
        assert_eq!(v.drops[0].percent, 25.0);
    }

    /// Twitch sometimes reports more minutes than required; a bar past 100%
    /// looks broken.
    #[test]
    fn percent_is_capped_at_a_hundred() {
        let v = view(&campaign(Duration::days(10), 60, 75));
        assert_eq!(v.drops[0].percent, 100.0);
    }

    #[test]
    fn a_zero_minute_requirement_does_not_divide_by_zero() {
        let v = view(&campaign(Duration::days(10), 0, 0));
        assert_eq!(v.drops[0].percent, 0.0);
    }

    /// A tier whose prerequisite is unclaimed cannot be earned yet, and the
    /// dashboard has to say so rather than showing it as simply "0 minutes".
    #[test]
    fn a_tier_is_locked_until_its_prerequisite_is_claimed() {
        let mut c = campaign(Duration::days(10), 60, 0);
        let first = c.drops[0].id.clone();
        c.drops.push(TimedDrop {
            id: DropId::from("d2"),
            name: "Tier 2".into(),
            benefits: vec![],
            required_minutes: 120,
            current_minutes: 0,
            claim_id: None,
            is_claimed: false,
            precondition_ids: vec![first],
        });

        let v = view(&c);
        assert!(!v.drops[0].locked);
        assert!(v.drops[1].locked, "tier 2 needs tier 1 claimed first");

        c.drops[0].is_claimed = true;
        let v = view(&c);
        assert!(!v.drops[1].locked, "unlocked once tier 1 is claimed");
    }

    #[test]
    fn exclusion_is_reported_by_campaign_and_by_game() {
        let c = campaign(Duration::days(10), 60, 0);
        let (mut excl, mut games, cur, live) = empty_ctx();

        excl.insert("c1".to_string());
        assert!(campaign_view(&c, &ctx(&excl, &games, &cur, &live)).excluded);

        excl.clear();
        games.insert("deadlock".to_string());
        assert!(
            campaign_view(&c, &ctx(&excl, &games, &cur, &live)).excluded,
            "excluding a game excludes its campaigns"
        );
    }

    #[test]
    fn the_current_campaign_is_marked() {
        let c = campaign(Duration::days(10), 60, 0);
        let (excl, games, _, live) = empty_ctx();
        let current = vec![CampaignId::from("c1")];
        assert!(campaign_view(&c, &ctx(&excl, &games, &current, &live)).is_current);
    }

    #[test]
    fn live_channel_counts_are_attached_by_campaign_id() {
        let c = campaign(Duration::days(10), 60, 0);
        let (excl, games, cur, mut live) = empty_ctx();
        live.insert("c1".to_string(), 7);
        assert_eq!(
            campaign_view(&c, &ctx(&excl, &games, &cur, &live)).live_channels,
            7
        );
    }

    #[test]
    fn the_active_drop_is_marked_and_others_are_not() {
        let c = campaign(Duration::days(10), 60, 10);
        let (excl, games, cur, live) = empty_ctx();
        let active = DropId::from("d1");
        let mut context = ctx(&excl, &games, &cur, &live);
        context.active_drop = Some(&active);
        assert!(campaign_view(&c, &context).drops[0].active);

        let other = DropId::from("nope");
        context.active_drop = Some(&other);
        assert!(!campaign_view(&c, &context).drops[0].active);
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let snap = Snapshot {
            account: "default".into(),
            login: Some("k4hvh".into()),
            strategy: "deadline".into(),
            state: "Mining".into(),
            paused: false,
            generated_at: now(),
            watching: Some(WatchingView {
                channel_login: "someone".into(),
                channel_display_name: "Someone".into(),
                stream_title: "playing".into(),
                viewers: 120,
                campaign: "Camp".into(),
                campaigns: vec!["Camp".into(), "Other".into()],
                reason: "deadline: advances 2 campaigns at once".into(),
                since: now(),
                drop_name: Some("Reward".into()),
                current_minutes: Some(23),
                required_minutes: Some(60),
                seconds_since_confirmation: Some(41),
                stalled: false,
            }),
            eligible: vec![view(&campaign(Duration::days(1), 60, 0))],
            rejected: vec![],
            live: vec![],
            settings: SettingsView::default(),
            strategies: vec![],
            health: HealthView::default(),
            totals: Totals::default(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.login.as_deref(), Some("k4hvh"));
        let w = back.watching.unwrap();
        assert_eq!(w.current_minutes, Some(23));
        assert_eq!(w.campaigns.len(), 2);
    }
}
