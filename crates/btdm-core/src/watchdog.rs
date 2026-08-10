//! Stall detection and the recovery ladder.
//!
//! A stall is defined only in terms of *server-confirmed* progress, so this
//! never fires on a healthy-but-quiet session and never stays silent through a
//! session that is earning nothing.
//!
//! When a stall is confirmed the ladder is worked cheapest-first. Each rung is
//! attempted once per stall episode, with a cooldown between rungs so recovery
//! cannot become a request storm. Reaching the end without recovering is itself
//! a reportable outcome rather than a silent give-up.

use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogConfig {
    /// No confirmation for this long is suspicious but not yet wrong.
    pub suspect_after: Duration,
    /// No confirmation for this long is a real failure.
    pub stalled_after: Duration,
    /// Minimum gap between escalation rungs.
    pub cooldown: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            suspect_after: Duration::seconds(150),
            stalled_after: Duration::seconds(360),
            cooldown: Duration::seconds(60),
        }
    }
}

/// The recovery ladder, cheapest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecoveryStep {
    /// Send another heartbeat immediately.
    ResendWatch,
    /// Re-fetch stream info. The broadcast ID changes when a stream restarts,
    /// and a stale one earns nothing while continuing to look healthy.
    RefreshStreamInfo,
    /// Ask Twitch again whether this channel can progress our drops.
    ReverifyEligibility,
    /// Give up on this channel and quarantine it.
    SwitchChannel,
    /// The campaign may have ended underneath us.
    ResyncCampaigns,
    /// The token may have been revoked.
    ValidateAuth,
    /// Nothing worked; stop trying this campaign and tell someone.
    QuarantineCampaign,
}

impl RecoveryStep {
    pub const LADDER: [RecoveryStep; 7] = [
        RecoveryStep::ResendWatch,
        RecoveryStep::RefreshStreamInfo,
        RecoveryStep::ReverifyEligibility,
        RecoveryStep::SwitchChannel,
        RecoveryStep::ResyncCampaigns,
        RecoveryStep::ValidateAuth,
        RecoveryStep::QuarantineCampaign,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Progress is confirming normally.
    Healthy,
    /// Quiet for longer than expected, but within tolerance.
    Suspect,
    /// Stalled; work this rung of the ladder.
    Escalate(RecoveryStep),
    /// Stalled, and waiting out the cooldown before the next rung.
    Waiting,
    /// The ladder is exhausted. Recovery failed and must be reported.
    Exhausted,
}

#[derive(Debug)]
pub struct Watchdog {
    config: WatchdogConfig,
    next_rung: usize,
    last_escalation: Option<DateTime<Utc>>,
}

impl Watchdog {
    pub fn new(config: WatchdogConfig) -> Self {
        Self {
            config,
            next_rung: 0,
            last_escalation: None,
        }
    }

    /// Called whenever progress is confirmed. Ends the current stall episode.
    pub fn on_progress(&mut self) {
        self.next_rung = 0;
        self.last_escalation = None;
    }

    /// Called when the miner moves to a different channel, which starts a fresh
    /// episode — the previous channel's failures say nothing about this one.
    pub fn reset(&mut self) {
        self.on_progress();
    }

    /// `since_confirmation` is time since the last server-confirmed progress,
    /// or since the watch session began if nothing has ever been confirmed.
    /// `None` means nothing is being watched.
    pub fn assess(
        &mut self,
        since_confirmation: Option<Duration>,
        now: DateTime<Utc>,
    ) -> Verdict {
        let Some(since) = since_confirmation else {
            return Verdict::Healthy;
        };

        if since < self.config.suspect_after {
            return Verdict::Healthy;
        }
        if since < self.config.stalled_after {
            return Verdict::Suspect;
        }

        if self.next_rung >= RecoveryStep::LADDER.len() {
            return Verdict::Exhausted;
        }

        // Space the rungs out so recovery cannot become a request storm.
        if let Some(last) = self.last_escalation
            && now - last < self.config.cooldown
        {
            return Verdict::Waiting;
        }

        let step = RecoveryStep::LADDER[self.next_rung];
        self.next_rung += 1;
        self.last_escalation = Some(now);
        Verdict::Escalate(step)
    }

    /// How far up the ladder the current episode has climbed.
    pub fn rungs_attempted(&self) -> usize {
        self.next_rung
    }
}

/// Channels and campaigns that have proven unproductive, with exponential
/// backoff so a temporarily broken channel is retried later rather than being
/// abandoned permanently.
#[derive(Debug, Default)]
pub struct Quarantine {
    entries: HashMap<String, Entry>,
    base: Duration,
    ceiling: Duration,
}

#[derive(Debug, Clone)]
struct Entry {
    until: DateTime<Utc>,
    strikes: u32,
}

impl Quarantine {
    pub fn new(base: Duration, ceiling: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            base,
            ceiling,
        }
    }

    /// Quarantine `key`, doubling the penalty for each repeat offence.
    pub fn punish(&mut self, key: &str, now: DateTime<Utc>) {
        let strikes = self.entries.get(key).map_or(1, |e| e.strikes.saturating_add(1));
        // Saturating shift: 2^30 minutes already exceeds any sane ceiling.
        let factor = 1i64 << strikes.min(30);
        let penalty = Duration::seconds(
            self.base
                .num_seconds()
                .saturating_mul(factor)
                .min(self.ceiling.num_seconds()),
        );
        self.entries.insert(
            key.to_owned(),
            Entry {
                until: now + penalty,
                strikes,
            },
        );
    }

    pub fn is_quarantined(&self, key: &str, now: DateTime<Utc>) -> bool {
        self.entries.get(key).is_some_and(|e| now < e.until)
    }

    /// Drops entries whose penalty has elapsed, so the map cannot grow without
    /// bound on a miner running for months.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        self.entries.retain(|_, e| now < e.until);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap() + Duration::seconds(secs)
    }

    fn watchdog() -> Watchdog {
        Watchdog::new(WatchdogConfig::default())
    }

    #[test]
    fn nothing_being_watched_is_not_a_stall() {
        assert_eq!(watchdog().assess(None, at(0)), Verdict::Healthy);
    }

    #[test]
    fn recent_confirmation_is_healthy() {
        assert_eq!(
            watchdog().assess(Some(Duration::seconds(30)), at(0)),
            Verdict::Healthy
        );
    }

    #[test]
    fn a_quiet_spell_is_suspect_before_it_is_a_failure() {
        assert_eq!(
            watchdog().assess(Some(Duration::seconds(200)), at(0)),
            Verdict::Suspect
        );
    }

    #[test]
    fn a_real_stall_starts_at_the_cheapest_rung() {
        assert_eq!(
            watchdog().assess(Some(Duration::seconds(400)), at(0)),
            Verdict::Escalate(RecoveryStep::ResendWatch)
        );
    }

    #[test]
    fn the_ladder_is_climbed_in_order_one_rung_at_a_time() {
        let mut w = watchdog();
        let stalled = Some(Duration::seconds(400));
        let mut seen = Vec::new();
        let mut t = 0;

        for _ in 0..RecoveryStep::LADDER.len() {
            match w.assess(stalled, at(t)) {
                Verdict::Escalate(step) => seen.push(step),
                other => panic!("expected an escalation at t={t}, got {other:?}"),
            }
            t += 61; // clear the cooldown
        }
        assert_eq!(seen, RecoveryStep::LADDER.to_vec());
    }

    /// Recovery must not become a request storm.
    #[test]
    fn rungs_are_spaced_by_the_cooldown() {
        let mut w = watchdog();
        let stalled = Some(Duration::seconds(400));

        assert!(matches!(w.assess(stalled, at(0)), Verdict::Escalate(_)));
        assert_eq!(w.assess(stalled, at(30)), Verdict::Waiting);
        assert!(matches!(w.assess(stalled, at(61)), Verdict::Escalate(_)));
    }

    /// Running out of options is a reportable outcome, not a silent give-up.
    #[test]
    fn exhausting_the_ladder_is_reported() {
        let mut w = watchdog();
        let stalled = Some(Duration::seconds(400));
        let mut t = 0;
        for _ in 0..RecoveryStep::LADDER.len() {
            w.assess(stalled, at(t));
            t += 61;
        }
        assert_eq!(w.assess(stalled, at(t)), Verdict::Exhausted);
        // And it stays exhausted rather than wrapping around to rung 0.
        assert_eq!(w.assess(stalled, at(t + 1000)), Verdict::Exhausted);
    }

    #[test]
    fn confirmed_progress_ends_the_episode_and_resets_the_ladder() {
        let mut w = watchdog();
        let stalled = Some(Duration::seconds(400));
        w.assess(stalled, at(0));
        w.assess(stalled, at(61));
        assert_eq!(w.rungs_attempted(), 2);

        w.on_progress();
        assert_eq!(w.rungs_attempted(), 0);
        assert_eq!(
            w.assess(stalled, at(200)),
            Verdict::Escalate(RecoveryStep::ResendWatch),
            "a new stall starts from the cheapest rung again"
        );
    }

    #[test]
    fn switching_channels_resets_the_ladder() {
        let mut w = watchdog();
        w.assess(Some(Duration::seconds(400)), at(0));
        w.reset();
        assert_eq!(w.rungs_attempted(), 0);
    }

    #[test]
    fn quarantine_expires_so_a_channel_is_retried_later() {
        let mut q = Quarantine::new(Duration::minutes(5), Duration::hours(2));
        q.punish("ch1", at(0));
        assert!(q.is_quarantined("ch1", at(60)));
        // First strike is 5 min * 2^1 = 10 min.
        assert!(!q.is_quarantined("ch1", at(700)));
    }

    #[test]
    fn repeat_offenders_are_quarantined_for_longer() {
        let mut q = Quarantine::new(Duration::minutes(5), Duration::hours(2));
        q.punish("ch1", at(0));
        let first_ok_at = (1..).map(|i| i * 60).find(|&t| !q.is_quarantined("ch1", at(t)));

        q.punish("ch1", at(0));
        let second_ok_at = (1..).map(|i| i * 60).find(|&t| !q.is_quarantined("ch1", at(t)));

        assert!(
            second_ok_at > first_ok_at,
            "a second failure must cost more than the first"
        );
    }

    #[test]
    fn quarantine_backoff_is_capped() {
        let mut q = Quarantine::new(Duration::minutes(5), Duration::hours(2));
        for _ in 0..50 {
            q.punish("ch1", at(0));
        }
        assert!(
            !q.is_quarantined("ch1", at(Duration::hours(2).num_seconds() + 1)),
            "backoff must not exceed the ceiling"
        );
    }

    #[test]
    fn an_unknown_key_is_not_quarantined() {
        let q = Quarantine::new(Duration::minutes(5), Duration::hours(2));
        assert!(!q.is_quarantined("never-seen", at(0)));
    }

    /// A months-long run must not accumulate expired entries forever.
    #[test]
    fn pruning_drops_expired_entries() {
        let mut q = Quarantine::new(Duration::minutes(5), Duration::hours(2));
        q.punish("ch1", at(0));
        q.punish("ch2", at(0));
        assert_eq!(q.len(), 2);

        q.prune(at(Duration::hours(3).num_seconds()));
        assert!(q.is_empty());
    }
}
