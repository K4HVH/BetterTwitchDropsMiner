//! Whether a campaign can actually be mined.
//!
//! Every rule here is derived from server-reported facts. Nothing is inferred
//! from local settings — that inference is exactly what made the reference
//! implementation mine campaigns it could never progress. Its `eligible`
//! property (inventory.py:396) returns a *settings flag* for any campaign
//! containing a badge or emote, skipping the account-link check entirely, so
//! enabling that option makes it grind at campaigns that can never advance.

use crate::event::RejectReason;
use crate::model::*;
use chrono::{DateTime, Utc};

pub fn assess(campaign: &DropCampaign, now: DateTime<Utc>) -> Result<(), RejectReason> {
    if now < campaign.starts_at {
        return Err(RejectReason::NotStarted);
    }
    if now >= campaign.ends_at {
        return Err(RejectReason::Expired);
    }
    // The server is the only authority on whether this account is linked.
    // There is deliberately no settings flag that can override this.
    if !campaign.is_account_linked {
        return Err(RejectReason::NotLinked);
    }
    if mineable_drops(campaign).is_empty() {
        // Distinguish "nothing left to win" from "nothing watching can win".
        // Both leave no mineable drops, but only one of them is an
        // achievement — reporting a subscription reward as fully claimed would
        // claim credit for something the account never got.
        let unclaimed_needs_subs = campaign
            .drops
            .iter()
            .filter(|d| !d.is_claimed)
            .any(|d| !d.is_earnable_by_watching());
        return Err(if unclaimed_needs_subs {
            RejectReason::SubscriptionRequired
        } else {
            RejectReason::AllDropsClaimed
        });
    }
    // Every remaining tier's own window has closed, so nothing here can be
    // earned again however long we watch.
    if mineable_drops(campaign)
        .iter()
        .all(|d| d.window_has_closed(now))
    {
        return Err(RejectReason::Expired);
    }
    Ok(())
}

/// Drops that can be progressed right now: unclaimed, inside their own time
/// window, and with every precondition tier already claimed.
pub fn mineable_drops_at(campaign: &DropCampaign, now: DateTime<Utc>) -> Vec<&TimedDrop> {
    mineable_drops(campaign)
        .into_iter()
        .filter(|d| d.is_open_at(now))
        .collect()
}

/// As [`mineable_drops_at`], ignoring per-drop windows.
pub fn mineable_drops(campaign: &DropCampaign) -> Vec<&TimedDrop> {
    let claimed: std::collections::HashSet<&DropId> = campaign
        .drops
        .iter()
        .filter(|d| d.is_claimed)
        .map(|d| &d.id)
        .collect();

    campaign
        .drops
        .iter()
        .filter(|d| !d.is_claimed)
        // A drop that wants a subscription is not earnable by watching, and
        // buying one is not something a miner gets to decide. Twitch ships
        // these through the same structure as watch-time drops, so without
        // this they are counted as work the miner can do and never does.
        .filter(|d| d.is_earnable_by_watching())
        .filter(|d| d.precondition_ids.iter().all(|p| claimed.contains(p)))
        .collect()
}

/// Minutes of watching still needed to finish a single drop, including every
/// tier it is gated behind.
///
/// Twitch expresses tiers two different ways. Most campaigns give each drop a
/// *cumulative* requirement (60, 120, 180 minutes of the same watching) and no
/// preconditions. Others gate a drop behind earlier ones, and then the
/// requirements are that drop's own — you must finish the predecessor first
/// and then watch again. Adding the worst predecessor chain handles both:
/// with no preconditions the chain contributes nothing and the cumulative
/// figures stand on their own.
fn total_remaining_minutes(campaign: &DropCampaign, drop: &TimedDrop) -> u32 {
    fn walk(
        campaign: &DropCampaign,
        drop: &TimedDrop,
        // The chain currently being followed. Cycle detection has to be
        // per-path, not a set shared across the whole recursion: with a
        // diamond (D behind B and C, both behind A) a shared set would mark A
        // visited on the first branch and contribute nothing on the second,
        // understating the total — and understating it makes a campaign look
        // *more* feasible than it is, which is the error this maths exists to
        // prevent.
        path: &mut Vec<DropId>,
    ) -> u32 {
        if path.contains(&drop.id) {
            return 0;
        }
        path.push(drop.id.clone());

        let worst_chain = drop
            .precondition_ids
            .iter()
            .filter_map(|id| campaign.drops.iter().find(|d| &d.id == id))
            // A precondition already claimed is behind us and costs nothing.
            .filter(|d| !d.is_claimed)
            .map(|d| walk(campaign, d, path))
            .max()
            .unwrap_or(0);

        path.pop();
        drop.remaining_minutes().saturating_add(worst_chain)
    }

    walk(campaign, drop, &mut Vec::new())
}

/// Minutes of watching still needed to finish everything left in the campaign.
///
/// Computed over every *unclaimed* drop, not only the ones minable right now.
/// Counting only the unlocked tiers made a 1h-then-5h campaign report that it
/// needed one hour, so a campaign that could not possibly be finished looked
/// comfortable — and this figure feeds the feasibility filter, the "at risk"
/// flag and the completionist strategy alike.
pub fn remaining_minutes(campaign: &DropCampaign) -> u32 {
    campaign
        .drops
        .iter()
        .filter(|d| !d.is_claimed)
        // Watching cannot finish a subscription tier however long it runs, so
        // counting one as outstanding work would make a campaign look longer
        // than it is — and, through `slack`, could declare an achievable
        // campaign impossible and drop it.
        .filter(|d| d.is_earnable_by_watching())
        .map(|d| total_remaining_minutes(campaign, d))
        .max()
        .unwrap_or(0)
}

/// As [`remaining_minutes`], but ignoring tiers whose own window has closed —
/// they can never be earned, so counting them would make a finishable campaign
/// look impossible.
pub fn remaining_minutes_at(campaign: &DropCampaign, now: DateTime<Utc>) -> u32 {
    campaign
        .drops
        .iter()
        .filter(|d| !d.is_claimed)
        .filter(|d| d.is_earnable_by_watching())
        .filter(|d| !d.window_has_closed(now))
        .map(|d| total_remaining_minutes(campaign, d))
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, h, 0, 0).unwrap()
    }

    fn campaign(linked: bool, benefit: BenefitKind, claimed: bool) -> DropCampaign {
        DropCampaign {
            id: CampaignId::from("c1"),
            name: "Camp".into(),
            game: Game {
                id: GameId::from("g1"),
                name: "Game".into(),
                slug: "game".into(),
                box_art_url: None,
            },
            starts_at: at(10),
            ends_at: at(20),
            is_account_linked: linked,
            allowed_channels: vec![],
            account_link_url: None,
            drops: vec![TimedDrop {
                id: DropId::from("d1"),
                name: "Drop".into(),
                benefits: vec![Benefit {
                    id: "b1".into(),
                    name: "Thing".into(),
                    kind: benefit,
                    image_url: None,
                }],
                required_minutes: 60,
                current_minutes: 0,
                claim_id: None,
                is_claimed: claimed,
                precondition_ids: vec![],
                starts_at: None,
                ends_at: None,
                required_subs: 0,
            }],
        }
    }

    #[test]
    fn a_linked_active_campaign_is_eligible() {
        assert_eq!(
            assess(&campaign(true, BenefitKind::Entitlement, false), at(15)),
            Ok(())
        );
    }

    /// The reference implementation's defect, pinned. Enabling its badges and
    /// emotes option made badge campaigns eligible *without* checking the
    /// account link, so it mined campaigns that could never progress.
    #[test]
    fn a_badge_campaign_is_still_rejected_when_the_account_is_not_linked() {
        assert_eq!(
            assess(&campaign(false, BenefitKind::Badge, false), at(15)),
            Err(RejectReason::NotLinked)
        );
        assert_eq!(
            assess(&campaign(false, BenefitKind::Emote, false), at(15)),
            Err(RejectReason::NotLinked)
        );
    }

    #[test]
    fn a_linked_badge_campaign_is_eligible() {
        assert_eq!(
            assess(&campaign(true, BenefitKind::Badge, false), at(15)),
            Ok(())
        );
    }

    #[test]
    fn campaigns_outside_their_window_are_rejected_with_the_reason() {
        assert_eq!(
            assess(&campaign(true, BenefitKind::Entitlement, false), at(9)),
            Err(RejectReason::NotStarted)
        );
        assert_eq!(
            assess(&campaign(true, BenefitKind::Entitlement, false), at(21)),
            Err(RejectReason::Expired)
        );
    }

    #[test]
    fn a_fully_claimed_campaign_is_rejected() {
        assert_eq!(
            assess(&campaign(true, BenefitKind::Entitlement, true), at(15)),
            Err(RejectReason::AllDropsClaimed)
        );
    }

    #[test]
    fn mineable_drops_skips_claimed_and_blocked_tiers() {
        let mut c = campaign(true, BenefitKind::Entitlement, false);
        c.drops.push(TimedDrop {
            id: DropId::from("d2"),
            name: "Tier 2".into(),
            benefits: vec![],
            required_minutes: 180,
            current_minutes: 0,
            claim_id: None,
            is_claimed: false,
            // Blocked until d1 is claimed.
            precondition_ids: vec![DropId::from("d1")],
            starts_at: None,
            ends_at: None,
            required_subs: 0,
        });
        let ids: Vec<_> = mineable_drops(&c).iter().map(|d| d.id.clone()).collect();
        assert_eq!(
            ids,
            vec![DropId::from("d1")],
            "tier 2 is gated behind tier 1"
        );

        c.drops[0].is_claimed = true;
        let ids: Vec<_> = mineable_drops(&c).iter().map(|d| d.id.clone()).collect();
        assert_eq!(
            ids,
            vec![DropId::from("d2")],
            "tier 2 unlocks once tier 1 is claimed"
        );
    }

    #[test]
    fn remaining_minutes_is_the_largest_reachable_tier_not_the_sum() {
        let mut c = campaign(true, BenefitKind::Entitlement, false);
        c.drops[0].current_minutes = 20; // 40 remaining of 60
        c.drops.push(TimedDrop {
            id: DropId::from("d2"),
            name: "Tier 2".into(),
            benefits: vec![],
            required_minutes: 180,
            current_minutes: 20, // 160 remaining
            claim_id: None,
            is_claimed: false,
            precondition_ids: vec![],
            starts_at: None,
            ends_at: None,
            required_subs: 0,
        });
        assert_eq!(remaining_minutes(&c), 160);
    }
}

#[cfg(test)]
mod remaining_minutes_tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, h, 0, 0).unwrap()
    }

    fn tier(id: &str, required: u32, current: u32, after: &[&str]) -> TimedDrop {
        TimedDrop {
            id: DropId::from(id),
            name: id.into(),
            benefits: vec![],
            required_minutes: required,
            current_minutes: current,
            claim_id: None,
            is_claimed: false,
            precondition_ids: after.iter().map(|s| DropId::from(*s)).collect(),
            starts_at: None,
            ends_at: None,
            required_subs: 0,
        }
    }

    fn with(drops: Vec<TimedDrop>) -> DropCampaign {
        DropCampaign {
            id: CampaignId::from("c1"),
            name: "Camp".into(),
            game: Game {
                id: GameId::from("g1"),
                name: "Game".into(),
                slug: "game".into(),
                box_art_url: None,
            },
            starts_at: at(10),
            ends_at: at(20),
            is_account_linked: true,
            allowed_channels: vec![],
            account_link_url: None,
            drops,
        }
    }

    /// The common shape: no preconditions, cumulative requirements. The last
    /// tier is the whole job.
    #[test]
    fn cumulative_tiers_report_the_largest_requirement() {
        let c = with(vec![
            tier("d1", 60, 30, &[]),
            tier("d2", 120, 30, &[]),
            tier("d3", 180, 30, &[]),
        ]);
        assert_eq!(remaining_minutes(&c), 150);
    }

    /// The bug this replaced. A tier locked behind an unclaimed one was
    /// excluded entirely, so a 1h-then-5h campaign claimed it needed one hour
    /// — and a campaign that could never be finished looked comfortable.
    #[test]
    fn a_locked_tier_still_counts_towards_the_work_remaining() {
        let c = with(vec![
            tier("d1", 60, 0, &[]),
            tier("d2", 300, 0, &["d1"]),
        ]);
        assert_eq!(
            remaining_minutes(&c),
            360,
            "an hour, then five more — not an hour"
        );
    }

    #[test]
    fn a_claimed_precondition_no_longer_costs_anything() {
        let mut c = with(vec![
            tier("d1", 60, 60, &[]),
            tier("d2", 300, 0, &["d1"]),
        ]);
        c.drops[0].is_claimed = true;
        assert_eq!(remaining_minutes(&c), 300);
    }

    #[test]
    fn a_chain_of_preconditions_adds_up() {
        let c = with(vec![
            tier("d1", 10, 0, &[]),
            tier("d2", 20, 0, &["d1"]),
            tier("d3", 30, 0, &["d2"]),
        ]);
        assert_eq!(remaining_minutes(&c), 60);
    }

    /// Where a drop is gated behind two, the longer branch decides.
    #[test]
    fn branching_preconditions_take_the_worst_branch() {
        let c = with(vec![
            tier("short", 10, 0, &[]),
            tier("long", 100, 0, &[]),
            tier("final", 5, 0, &["short", "long"]),
        ]);
        assert_eq!(remaining_minutes(&c), 105);
    }

    #[test]
    fn everything_claimed_needs_no_more_watching() {
        let mut c = with(vec![tier("d1", 60, 60, &[])]);
        c.drops[0].is_claimed = true;
        assert_eq!(remaining_minutes(&c), 0);
    }

    /// Malformed data must not hang the miner.
    #[test]
    fn a_precondition_cycle_terminates() {
        let c = with(vec![
            tier("a", 10, 0, &["b"]),
            tier("b", 10, 0, &["a"]),
        ]);
        let _ = remaining_minutes(&c);
    }

    #[test]
    fn progress_already_made_is_deducted_along_the_chain() {
        let c = with(vec![
            tier("d1", 60, 45, &[]),
            tier("d2", 300, 0, &["d1"]),
        ]);
        assert_eq!(remaining_minutes(&c), 315, "15 left on tier 1, then 300");
    }
}

#[cfg(test)]
mod diamond_tests {
    use super::*;
    use chrono::TimeZone;

    fn tier(id: &str, required: u32, after: &[&str]) -> TimedDrop {
        TimedDrop {
            id: DropId::from(id),
            name: id.into(),
            benefits: vec![],
            required_minutes: required,
            current_minutes: 0,
            claim_id: None,
            is_claimed: false,
            precondition_ids: after.iter().map(|s| DropId::from(*s)).collect(),
            starts_at: None,
            ends_at: None,
            required_subs: 0,
        }
    }

    fn with(drops: Vec<TimedDrop>) -> DropCampaign {
        DropCampaign {
            id: CampaignId::from("c1"),
            name: "Camp".into(),
            game: Game {
                id: GameId::from("g1"),
                name: "Game".into(),
                slug: "game".into(),
                box_art_url: None,
            },
            starts_at: Utc.with_ymd_and_hms(2026, 8, 10, 10, 0, 0).unwrap(),
            ends_at: Utc.with_ymd_and_hms(2026, 8, 10, 20, 0, 0).unwrap(),
            is_account_linked: true,
            allowed_channels: vec![],
            account_link_url: None,
            drops,
        }
    }

    /// A shared visited set across the whole recursion marks a shared ancestor
    /// as counted on the first branch it appears in, so the second branch
    /// contributes nothing. That *understates* the work remaining, which makes
    /// an impossible campaign look achievable — the exact failure this maths
    /// exists to catch.
    #[test]
    fn a_diamond_dependency_counts_the_shared_ancestor_on_the_worst_branch() {
        // D is behind B and C; both are behind A.
        let c = with(vec![
            tier("A", 100, &[]),
            tier("B", 1, &["A"]),
            tier("C", 50, &["A"]),
            tier("D", 5, &["B", "C"]),
        ]);

        // Worst path is D -> C -> A: 5 + 50 + 100.
        assert_eq!(
            remaining_minutes(&c),
            155,
            "the shared ancestor must count on whichever branch is worst"
        );
    }

    /// The same shape, with the long branch first, to prove the answer does
    /// not depend on the order Twitch happens to list preconditions in.
    #[test]
    fn the_result_does_not_depend_on_precondition_order() {
        let a = with(vec![
            tier("A", 100, &[]),
            tier("B", 1, &["A"]),
            tier("C", 50, &["A"]),
            tier("D", 5, &["B", "C"]),
        ]);
        let b = with(vec![
            tier("A", 100, &[]),
            tier("C", 50, &["A"]),
            tier("B", 1, &["A"]),
            tier("D", 5, &["C", "B"]),
        ]);
        assert_eq!(remaining_minutes(&a), remaining_minutes(&b));
    }

    #[test]
    fn a_cycle_still_terminates() {
        let c = with(vec![tier("a", 10, &["b"]), tier("b", 10, &["a"])]);
        let _ = remaining_minutes(&c);
    }

    /// A deep chain must not be truncated by the cycle guard.
    #[test]
    fn a_long_chain_is_summed_in_full() {
        let mut drops = vec![tier("t0", 10, &[])];
        for i in 1..10 {
            let prev = format!("t{}", i - 1);
            drops.push(tier(&format!("t{i}"), 10, &[&prev]));
        }
        assert_eq!(remaining_minutes(&with(drops)), 100);
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, h, 0, 0).unwrap()
    }

    fn windowed(id: &str, required: u32, from: Option<u32>, until: Option<u32>) -> TimedDrop {
        TimedDrop {
            id: DropId::from(id),
            name: id.into(),
            benefits: vec![],
            required_minutes: required,
            current_minutes: 0,
            claim_id: None,
            is_claimed: false,
            precondition_ids: vec![],
            starts_at: from.map(at),
            ends_at: until.map(at),
            required_subs: 0,
        }
    }

    fn with(drops: Vec<TimedDrop>) -> DropCampaign {
        DropCampaign {
            id: CampaignId::from("c1"),
            name: "Camp".into(),
            game: Game {
                id: GameId::from("g1"),
                name: "Game".into(),
                slug: "game".into(),
                box_art_url: None,
            },
            starts_at: at(0),
            ends_at: at(23),
            is_account_linked: true,
            allowed_channels: vec![],
            account_link_url: None,
            drops,
        }
    }

    /// A tier that only opens next week is not mineable today, however much
    /// the campaign as a whole is.
    #[test]
    fn a_tier_whose_window_has_not_opened_is_not_mineable_yet() {
        let c = with(vec![windowed("later", 60, Some(20), None)]);
        assert!(mineable_drops_at(&c, at(12)).is_empty());
        assert_eq!(mineable_drops_at(&c, at(21)).len(), 1);
    }

    /// ...but it is still work the campaign requires, so it must count towards
    /// the total. Otherwise a campaign looks finishable when it is not.
    #[test]
    fn a_tier_that_has_not_opened_still_counts_towards_the_work_left() {
        let c = with(vec![windowed("later", 300, Some(20), None)]);
        assert_eq!(remaining_minutes_at(&c, at(12)), 300);
    }

    /// A tier whose window has closed can never be earned, so counting it
    /// would make a finishable campaign look impossible.
    #[test]
    fn a_tier_whose_window_has_closed_stops_counting() {
        let c = with(vec![
            windowed("gone", 600, None, Some(10)),
            windowed("open", 30, None, None),
        ]);
        assert_eq!(
            remaining_minutes_at(&c, at(12)),
            30,
            "the missed tier is lost, not outstanding"
        );
        // Without the window it would have dominated.
        assert_eq!(remaining_minutes(&c), 600);
    }

    /// A campaign whose every remaining tier has closed cannot be mined at all.
    #[test]
    fn a_campaign_whose_tiers_have_all_closed_is_rejected() {
        let c = with(vec![windowed("gone", 60, None, Some(10))]);
        assert_eq!(assess(&c, at(12)), Err(RejectReason::Expired));
    }

    #[test]
    fn a_drop_with_no_window_is_always_open() {
        let d = windowed("any", 60, None, None);
        assert!(d.is_open_at(at(0)));
        assert!(d.is_open_at(at(23)));
        assert!(!d.window_has_closed(at(23)));
    }
}
