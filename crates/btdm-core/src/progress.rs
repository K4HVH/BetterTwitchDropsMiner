//! Server-confirmed progress.
//!
//! The reference implementation shows a locally-ticking countdown that advances
//! whether or not Twitch is counting anything, which is why its users cannot
//! tell mining from not-mining. Nothing here advances without a server report,
//! and `since_last_confirmation` turns a stall into a measurable quantity.

use crate::api::DropProgress;
use crate::model::DropId;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedProgress {
    pub current_minutes: u32,
    pub required_minutes: u32,
    /// When this figure last actually increased.
    pub at: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct ProgressTracker {
    drops: HashMap<DropId, ConfirmedProgress>,
    last_advance: Option<DateTime<Utc>>,
    /// When the current watch session began, so a stall can be measured even
    /// before the first confirmation ever arrives.
    started_at: Option<DateTime<Utc>>,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the beginning of a watch session. Without this, a channel that
    /// never confirms anything would report "no stall" forever, because there
    /// would be no clock to measure against.
    pub fn begin(&mut self, at: DateTime<Utc>) {
        self.started_at = Some(at);
    }

    /// Returns `true` only when this report advanced a drop. Re-reports and
    /// backwards readings return `false` and leave the confirmed figure alone.
    pub fn record(&mut self, p: DropProgress, at: DateTime<Utc>) -> bool {
        let advanced = match self.drops.get(&p.drop_id) {
            Some(existing) => p.current_minutes > existing.current_minutes,
            None => true,
        };

        if advanced {
            self.drops.insert(
                p.drop_id,
                ConfirmedProgress {
                    current_minutes: p.current_minutes,
                    required_minutes: p.required_minutes,
                    at,
                },
            );
            self.last_advance = Some(at);
        }
        advanced
    }

    pub fn confirmed(&self, drop: &DropId) -> Option<&ConfirmedProgress> {
        self.drops.get(drop)
    }

    /// How long since anything actually advanced, falling back to how long
    /// since the session began when nothing has ever been confirmed.
    ///
    /// A channel that has never confirmed a single minute is the *worst* case,
    /// not an exempt one — returning `None` there would let the exact failure
    /// this project exists to catch slip through unmeasured.
    pub fn since_last_confirmation(&self, now: DateTime<Utc>) -> Option<Duration> {
        self.last_advance.or(self.started_at).map(|t| now - t)
    }

    /// True once nothing has been confirmed for longer than `threshold`.
    pub fn is_stalled(&self, now: DateTime<Utc>, threshold: Duration) -> bool {
        self.since_last_confirmation(now)
            .is_some_and(|d| d >= threshold)
    }

    pub fn has_confirmation(&self) -> bool {
        self.last_advance.is_some()
    }

    /// Forget everything — used when switching channels, since progress on the
    /// old channel says nothing about the new one.
    pub fn reset(&mut self) {
        self.drops.clear();
        self.last_advance = None;
        self.started_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, 12, m, 0).unwrap()
    }

    fn progress(current: u32) -> DropProgress {
        DropProgress {
            drop_id: DropId::from("d1"),
            current_minutes: current,
            required_minutes: 60,
        }
    }

    #[test]
    fn the_first_report_counts_as_advancing() {
        let mut t = ProgressTracker::new();
        assert!(t.record(progress(5), at(0)));
        assert_eq!(t.confirmed(&DropId::from("d1")).unwrap().current_minutes, 5);
    }

    #[test]
    fn a_higher_reading_advances() {
        let mut t = ProgressTracker::new();
        t.record(progress(5), at(0));
        assert!(t.record(progress(6), at(1)));
    }

    #[test]
    fn a_repeated_reading_does_not_advance() {
        let mut t = ProgressTracker::new();
        t.record(progress(5), at(0));
        assert!(
            !t.record(progress(5), at(1)),
            "same minute count is not progress"
        );
    }

    /// Twitch sometimes reports progress for the wrong drop, or replays an old
    /// value. Going backwards must never be mistaken for progress, and must not
    /// corrupt the confirmed figure.
    #[test]
    fn a_lower_reading_is_ignored() {
        let mut t = ProgressTracker::new();
        t.record(progress(10), at(0));
        assert!(!t.record(progress(4), at(1)));
        assert_eq!(t.confirmed(&DropId::from("d1")).unwrap().current_minutes, 10);
    }

    #[test]
    fn tracks_time_since_the_last_real_advance() {
        let mut t = ProgressTracker::new();
        t.record(progress(5), at(0));
        assert_eq!(t.since_last_confirmation(at(3)), Some(Duration::minutes(3)));

        // A non-advancing report must not reset the clock — that is precisely
        // how a stall stays invisible.
        t.record(progress(5), at(3));
        assert_eq!(t.since_last_confirmation(at(6)), Some(Duration::minutes(6)));

        t.record(progress(6), at(7));
        assert_eq!(t.since_last_confirmation(at(8)), Some(Duration::minutes(1)));
    }

    /// A channel that never confirms anything is the worst case, so it must be
    /// measured from the session start rather than exempted.
    #[test]
    fn a_session_that_never_confirms_still_measures_a_stall() {
        let mut t = ProgressTracker::new();
        assert_eq!(t.since_last_confirmation(at(5)), None, "no session yet");

        t.begin(at(0));
        assert_eq!(t.since_last_confirmation(at(5)), Some(Duration::minutes(5)));
        assert!(t.is_stalled(at(7), Duration::minutes(6)));
        assert!(!t.has_confirmation());
    }

    #[test]
    fn is_stalled_only_once_the_threshold_is_reached() {
        let mut t = ProgressTracker::new();
        t.begin(at(0));
        t.record(progress(5), at(1));
        assert!(!t.is_stalled(at(5), Duration::minutes(6)));
        assert!(t.is_stalled(at(8), Duration::minutes(6)));
    }

    #[test]
    fn drops_are_tracked_independently() {
        let mut t = ProgressTracker::new();
        t.record(progress(5), at(0));
        t.record(
            DropProgress {
                drop_id: DropId::from("d2"),
                current_minutes: 1,
                required_minutes: 30,
            },
            at(1),
        );
        assert_eq!(t.confirmed(&DropId::from("d1")).unwrap().current_minutes, 5);
        assert_eq!(t.confirmed(&DropId::from("d2")).unwrap().current_minutes, 1);
    }

    #[test]
    fn reset_clears_the_session_clock_too() {
        let mut t = ProgressTracker::new();
        t.begin(at(0));
        t.record(progress(5), at(1));
        t.reset();
        assert_eq!(t.since_last_confirmation(at(9)), None);
    }
}
