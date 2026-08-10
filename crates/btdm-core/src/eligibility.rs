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

/// Minutes of watching still needed to finish every reachable drop in the
/// campaign. Tiers are cumulative, so this is the largest remaining figure
/// rather than their sum.
pub fn remaining_minutes(campaign: &DropCampaign) -> u32 {
    mineable_drops(campaign)
        .iter()
        .map(|d| d.remaining_minutes())
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
