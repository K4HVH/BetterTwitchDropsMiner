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
        return Err(RejectReason::AllDropsClaimed);
    }
    Ok(())
}

/// Drops that can be progressed right now: unclaimed, and with every
/// precondition tier already claimed.
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
        // Guards against a precondition cycle. Twitch should never send one,
        // but a miner that hangs on malformed data is worse than one that
        // slightly misjudges a deadline.
        seen: &mut std::collections::HashSet<DropId>,
    ) -> u32 {
        if !seen.insert(drop.id.clone()) {
            return 0;
        }
        let worst_chain = drop
            .precondition_ids
            .iter()
            .filter_map(|id| campaign.drops.iter().find(|d| &d.id == id))
            // A precondition already claimed is behind us and costs nothing.
            .filter(|d| !d.is_claimed)
            .map(|d| walk(campaign, d, seen))
            .max()
            .unwrap_or(0);
        drop.remaining_minutes().saturating_add(worst_chain)
    }

    walk(campaign, drop, &mut std::collections::HashSet::new())
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
