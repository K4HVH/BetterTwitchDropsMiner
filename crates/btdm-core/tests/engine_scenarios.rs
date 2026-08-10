mod support;

use btdm_core::api::{ApiError, DropProgress};
use btdm_core::engine::{Engine, EngineConfig, MiningState};
use btdm_core::event::{EventKind, RejectReason};
use btdm_core::model::*;
use chrono::{TimeZone, Utc};
use support::*;

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap()
}

fn engine(fake: FakeTwitch) -> Engine<FakeTwitch> {
    Engine::new(fake, EngineConfig::default())
}

/// One eligible campaign, one channel that can progress it.
fn happy_path() -> FakeTwitch {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1", true))];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1")]);
    fake
}

#[tokio::test]
async fn selects_an_eligible_channel_and_starts_watching() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    let selected = e.select_channel(now()).await.unwrap();

    assert!(selected.is_some());
    assert_eq!(selected.unwrap().0.login, "streamer1");
    assert_eq!(e.state(), MiningState::Establishing);
}

/// The reference implementation's defect at engine level: an unlinked campaign
/// is never selected, and the rejection is reported rather than silently
/// dropped.
#[tokio::test]
async fn never_selects_a_campaign_the_account_is_not_linked_to() {
    let fake = happy_path();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", false, vec![drop("d1", 60, 0)])];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.select_channel(now()).await.unwrap().is_none());

    let rejected = e.drain_events().into_iter().any(|ev| {
        matches!(
            ev.kind,
            EventKind::CampaignRejected {
                reason: RejectReason::NotLinked,
                ..
            }
        )
    });
    assert!(rejected, "the rejection must be reported, not silent");
}

/// A campaign the account has never watched only appears on the dashboard, not
/// in the inventory. Merging the two is what lets the miner *start* campaigns
/// rather than only continue them.
#[tokio::test]
async fn starts_a_campaign_that_is_not_yet_in_the_inventory() {
    let fake = FakeTwitch::default();
    // Inventory is empty — nothing has ever been mined on this account.
    let listed = campaign("c-new", true, vec![]);
    let full = campaign("c-new", true, vec![drop("d1", 60, 0)]);
    *fake.offered.lock().unwrap() = vec![listed];
    fake.details.lock().unwrap().insert("c-new".into(), full);
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1", true))];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert_eq!(e.campaigns().len(), 1, "dashboard campaign must be picked up");

    let selected = e.select_channel(now()).await.unwrap();
    assert!(selected.is_some(), "a brand-new campaign must be minable");
}

#[tokio::test]
async fn does_not_refetch_details_for_campaigns_already_in_the_inventory() {
    let fake = happy_path();
    *fake.offered.lock().unwrap() = vec![campaign("c1", true, vec![])];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert_eq!(
        *e.api().detail_fetches.lock().unwrap(),
        0,
        "inventory data is already complete"
    );
}

#[tokio::test]
async fn does_not_spend_requests_on_already_expired_campaigns() {
    let fake = FakeTwitch::default();
    let mut expired = campaign("c-old", true, vec![]);
    expired.ends_at = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    *fake.offered.lock().unwrap() = vec![expired];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert_eq!(*e.api().detail_fetches.lock().unwrap(), 0);
}

/// A channel Twitch says progresses nothing must not be chosen while a better
/// option exists.
#[tokio::test]
async fn prefers_a_channel_that_actually_progresses_our_drops() {
    let fake = happy_path();
    *fake.channels.lock().unwrap() = vec![
        (channel("ch-dead", "deadstreamer"), stream("bc-0", true)),
        (channel("ch1", "streamer1"), stream("bc-1", true)),
    ];
    // Only ch1 is in `available`.

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let (chosen, _, _) = e.select_channel(now()).await.unwrap().unwrap();
    assert_eq!(chosen.login, "streamer1");
}

/// Trusting the availability check absolutely would turn one bad Twitch
/// response into a miner that mines nothing at all — worse than the bug it
/// exists to prevent. It falls back, loudly.
#[tokio::test]
async fn falls_back_to_the_directory_tag_when_nothing_passes_the_availability_check() {
    let fake = happy_path();
    fake.available.lock().unwrap().clear();

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let selected = e.select_channel(now()).await.unwrap();
    assert!(
        selected.is_some(),
        "a drops-enabled channel is better than mining nothing"
    );
}

#[tokio::test]
async fn a_failing_availability_check_does_not_exclude_a_drops_enabled_channel() {
    let fake = happy_path();
    fake.available.lock().unwrap().clear();
    *fake.fail_available_drops.lock().unwrap() = true;

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.select_channel(now()).await.unwrap().is_some());
}

#[tokio::test]
async fn mines_nothing_when_no_channel_is_even_drops_enabled() {
    let fake = happy_path();
    fake.available.lock().unwrap().clear();
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1", false))];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.select_channel(now()).await.unwrap().is_none());
}

#[tokio::test]
async fn respects_a_campaign_channel_allow_list() {
    let fake = happy_path();
    let mut c = campaign("c1", true, vec![drop("d1", 60, 0)]);
    c.allowed_channels = vec![channel("ch-allowed", "allowed")];
    *fake.inventory.lock().unwrap() = vec![c];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1", true))];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(
        e.select_channel(now()).await.unwrap().is_none(),
        "streamer1 is not on the allow-list"
    );
}

#[tokio::test]
async fn a_tick_sends_a_watch_heartbeat() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    assert_eq!(e.api().watches_sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn heartbeats_are_not_resent_before_the_interval_elapses() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();
    e.tick(now() + chrono::Duration::seconds(5)).await.unwrap();

    assert_eq!(e.api().watches_sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn confirmed_progress_is_reported_but_repeats_are_not() {
    let fake = happy_path();
    *fake.progress.lock().unwrap() = Some(DropProgress {
        drop_id: DropId::from("d1"),
        current_minutes: 1,
        required_minutes: 60,
    });

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    e.tick(now()).await.unwrap();
    let confirmations = e
        .drain_events()
        .into_iter()
        .filter(|ev| matches!(ev.kind, EventKind::ProgressConfirmed { .. }))
        .count();
    assert_eq!(confirmations, 1);
    assert_eq!(e.state(), MiningState::Mining);

    // Advance past the poll interval so a poll definitely happens, otherwise
    // this would pass merely because no poll was due.
    e.tick(now() + chrono::Duration::seconds(150)).await.unwrap();
    let confirmations = e
        .drain_events()
        .into_iter()
        .filter(|ev| matches!(ev.kind, EventKind::ProgressConfirmed { .. }))
        .count();
    assert_eq!(confirmations, 0, "a repeated reading is not progress");
}

/// The whole promise of the project: a channel that never confirms anything is
/// measurably stalled, not indistinguishable from a healthy one.
#[tokio::test]
async fn a_channel_that_never_confirms_is_visibly_stalled() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    let stalled_for = e
        .since_last_confirmation(now() + chrono::Duration::minutes(7))
        .expect("a watching session always has a stall clock");
    assert_eq!(stalled_for, chrono::Duration::minutes(7));
    assert_eq!(e.state(), MiningState::Establishing);
}

#[tokio::test]
async fn claims_a_drop_as_soon_as_it_becomes_claimable() {
    let fake = happy_path();
    let mut d = drop("d1", 60, 60);
    d.claim_id = Some("inst-1".into());
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![d])];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    assert_eq!(*e.api().claims.lock().unwrap(), vec!["inst-1".to_string()]);
    assert!(
        e.drain_events()
            .into_iter()
            .any(|ev| matches!(ev.kind, EventKind::DropClaimed { .. })),
        "claiming must be reported"
    );
}

#[tokio::test]
async fn a_drop_is_not_claimed_twice() {
    let fake = happy_path();
    let mut d = drop("d1", 60, 60);
    d.claim_id = Some("inst-1".into());
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![d])];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.tick(now()).await.unwrap();
    e.tick(now() + chrono::Duration::seconds(120)).await.unwrap();

    assert_eq!(e.api().claims.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_websocket_claim_notification_triggers_a_claim() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();

    e.on_drop_claimable(&DropId::from("d1"), "inst-ws");
    e.tick(now()).await.unwrap();

    assert_eq!(*e.api().claims.lock().unwrap(), vec!["inst-ws".to_string()]);
}

#[tokio::test]
async fn pushed_progress_updates_the_campaign_view() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d1"),
            current_minutes: 42,
            required_minutes: 60,
        },
        now(),
    );

    assert_eq!(e.campaigns()[0].drops[0].current_minutes, 42);
    assert_eq!(e.confirmed(&DropId::from("d1")).unwrap().current_minutes, 42);
}

#[tokio::test]
async fn an_expired_token_surfaces_as_unauthorized_rather_than_a_silent_stop() {
    let fake = FakeTwitch::default();
    *fake.fail_with.lock().unwrap() = Some(ApiError::Unauthorized);

    let mut e = engine(fake);
    assert!(matches!(e.sync(now()).await, Err(ApiError::Unauthorized)));
}

/// If the dashboard query dies we must still mine what the inventory knows,
/// rather than failing the whole sync.
#[tokio::test]
async fn a_dashboard_failure_does_not_lose_the_inventory() {
    let fake = happy_path();

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert_eq!(e.campaigns().len(), 1);
}

#[tokio::test]
async fn switching_channels_clears_progress_from_the_old_one() {
    let fake = happy_path();
    *fake.progress.lock().unwrap() = Some(DropProgress {
        drop_id: DropId::from("d1"),
        current_minutes: 5,
        required_minutes: 60,
    });

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();
    assert!(e.since_last_confirmation(now()).is_some());

    e.switch_away(now());
    assert_eq!(
        e.since_last_confirmation(now()),
        None,
        "progress on the previous channel says nothing about the next"
    );
    assert_eq!(e.state(), MiningState::Idle);
}

#[tokio::test]
async fn campaigns_are_ordered_by_deadline_so_behaviour_is_reproducible() {
    let fake = FakeTwitch::default();
    let mut late = campaign("c-late", true, vec![drop("d2", 60, 0)]);
    late.ends_at = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
    let mut soon = campaign("c-soon", true, vec![drop("d1", 60, 0)]);
    soon.ends_at = Utc.with_ymd_and_hms(2026, 8, 15, 0, 0, 0).unwrap();
    *fake.inventory.lock().unwrap() = vec![late, soon];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert_eq!(e.campaigns()[0].id, CampaignId::from("c-soon"));
}

// ---------------------------------------------------------------------------
// Strategy, watchdog and quarantine integration
// ---------------------------------------------------------------------------

use btdm_core::watchdog::{RecoveryStep, Verdict};

fn engine_with(fake: FakeTwitch, mutate: impl FnOnce(&mut EngineConfig)) -> Engine<FakeTwitch> {
    let mut config = EngineConfig::default();
    mutate(&mut config);
    Engine::new(fake, config)
}

#[tokio::test]
async fn the_default_strategy_is_deadline() {
    assert_eq!(engine(happy_path()).strategy_name(), "deadline");
}

#[tokio::test]
async fn an_unknown_strategy_falls_back_to_deadline_rather_than_failing() {
    let e = engine_with(happy_path(), |c| c.strategy = "nonsense".into());
    assert_eq!(e.strategy_name(), "deadline");
}

/// A campaign that cannot finish before it expires must not consume the single
/// watch slot, and the reason must be reported.
#[tokio::test]
async fn an_infeasible_campaign_is_skipped_with_a_reason() {
    let fake = FakeTwitch::default();
    let mut c = campaign("c1", true, vec![drop("d1", 600, 0)]);
    c.ends_at = now() + chrono::Duration::hours(1); // 600 min needed, 60 available
    *fake.inventory.lock().unwrap() = vec![c];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1", true))];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.select_channel(now()).await.unwrap().is_none());

    assert!(
        e.drain_events().into_iter().any(|ev| matches!(
            ev.kind,
            EventKind::CampaignRejected { reason: RejectReason::Infeasible, .. }
        )),
        "an impossible campaign must say why it was skipped"
    );
}

#[tokio::test]
async fn the_deadline_strategy_prefers_the_campaign_closest_to_expiry() {
    let fake = FakeTwitch::default();
    let mut late = campaign("c-late", true, vec![drop("d-late", 60, 0)]);
    late.ends_at = now() + chrono::Duration::days(20);
    let mut soon = campaign("c-soon", true, vec![drop("d-soon", 60, 0)]);
    soon.ends_at = now() + chrono::Duration::hours(4);
    *fake.inventory.lock().unwrap() = vec![late, soon];

    *fake.channels.lock().unwrap() = vec![
        (channel("ch-late", "latestreamer"), stream("bc-1", true)),
        (channel("ch-soon", "soonstreamer"), stream("bc-2", true)),
    ];
    fake.available
        .lock()
        .unwrap()
        .insert("ch-late".into(), vec![DropId::from("d-late")]);
    fake.available
        .lock()
        .unwrap()
        .insert("ch-soon".into(), vec![DropId::from("d-soon")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let (chosen, _, _) = e.select_channel(now()).await.unwrap().unwrap();
    assert_eq!(chosen.login, "soonstreamer");
}

#[tokio::test]
async fn a_favourite_channel_wins_an_otherwise_equal_choice() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![
        campaign("c-a", true, vec![drop("d-a", 60, 0)]),
        campaign("c-b", true, vec![drop("d-b", 60, 0)]),
    ];
    *fake.channels.lock().unwrap() = vec![
        (channel("ch-a", "plain"), stream("bc-1", true)),
        (channel("ch-b", "beloved"), stream("bc-2", true)),
    ];
    fake.available
        .lock()
        .unwrap()
        .insert("ch-a".into(), vec![DropId::from("d-a")]);
    fake.available
        .lock()
        .unwrap()
        .insert("ch-b".into(), vec![DropId::from("d-b")]);

    let mut e = engine_with(fake, |c| c.favourites = vec!["Beloved".into()]);
    e.sync(now()).await.unwrap();
    let (chosen, _, _) = e.select_channel(now()).await.unwrap().unwrap();
    assert_eq!(chosen.login, "beloved");
}

#[tokio::test]
async fn a_healthy_session_is_not_escalated() {
    let fake = happy_path();
    *fake.progress.lock().unwrap() = Some(DropProgress {
        drop_id: DropId::from("d1"),
        current_minutes: 1,
        required_minutes: 60,
    });

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    assert_eq!(e.assess_health(now()), Verdict::Healthy);
}

/// The headline promise: a channel that confirms nothing is escalated rather
/// than mined forever.
#[tokio::test]
async fn a_silent_channel_escalates_through_the_ladder() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    assert_eq!(e.assess_health(now() + chrono::Duration::seconds(60)), Verdict::Healthy);
    assert_eq!(e.assess_health(now() + chrono::Duration::seconds(200)), Verdict::Suspect);
    assert_eq!(
        e.assess_health(now() + chrono::Duration::seconds(400)),
        Verdict::Escalate(RecoveryStep::ResendWatch)
    );
}

#[tokio::test]
async fn confirmed_progress_clears_an_in_flight_stall() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    let stalled = now() + chrono::Duration::seconds(400);
    assert!(matches!(e.assess_health(stalled), Verdict::Escalate(_)));

    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d1"),
            current_minutes: 1,
            required_minutes: 60,
        },
        stalled,
    );
    assert_eq!(e.assess_health(stalled), Verdict::Healthy);
}

#[tokio::test]
async fn a_quarantined_channel_is_not_reselected() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    assert!(e.current_channel().is_some());

    e.quarantine_current(now());
    assert!(e.current_channel().is_none());

    assert!(
        e.select_channel(now()).await.unwrap().is_none(),
        "the quarantined channel must not be picked straight back up"
    );
}

/// Quarantine is a cooldown, not a life sentence.
#[tokio::test]
async fn a_quarantined_channel_becomes_available_again_later() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.quarantine_current(now());

    let much_later = now() + chrono::Duration::hours(3);
    e.sync(much_later).await.unwrap();
    assert!(e.select_channel(much_later).await.unwrap().is_some());
}

#[tokio::test]
async fn the_availability_check_budget_bounds_requests_per_pass() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    let mut channels = Vec::new();
    for i in 0..100 {
        channels.push((
            channel(&format!("ch{i}"), &format!("streamer{i}")),
            stream("bc", true),
        ));
    }
    *fake.channels.lock().unwrap() = channels;
    // None are in `available`, so every one costs a check.

    let mut e = engine_with(fake, |c| c.max_availability_checks = 5);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    // Budget respected; without it this would be 100 sequential requests.
}


/// Twitch reports drop events per *account*, not per session. If the user is
/// also watching in a browser, those events arrive here too. Counting them
/// would reset the stall clock while this miner earned nothing — the exact
/// failure this project exists to prevent.
#[tokio::test]
async fn progress_for_a_drop_we_are_not_mining_is_ignored() {
    let fake = happy_path();
    *fake.inventory.lock().unwrap() = vec![
        campaign("c1", true, vec![drop("d1", 60, 0)]),
        campaign("c-elsewhere", true, vec![drop("d-elsewhere", 60, 0)]),
    ];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.drain_events();

    let later = now() + chrono::Duration::seconds(300);
    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d-elsewhere"),
            current_minutes: 42,
            required_minutes: 60,
        },
        later,
    );

    assert!(
        !e.drain_events()
            .into_iter()
            .any(|ev| matches!(ev.kind, EventKind::ProgressConfirmed { .. })),
        "another campaign's progress must not be reported as ours"
    );
    assert_eq!(
        e.since_last_confirmation(later),
        Some(chrono::Duration::seconds(300)),
        "the stall clock must keep running — this session has confirmed nothing"
    );
}

#[tokio::test]
async fn progress_arriving_while_idle_is_ignored() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    // Deliberately no select_channel: nothing is being watched.

    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d1"),
            current_minutes: 5,
            required_minutes: 60,
        },
        now(),
    );
    assert!(e.active_drop().is_none());
}

/// A stall must not be cleared by unrelated progress.
#[tokio::test]
async fn an_unrelated_drop_cannot_clear_a_stall() {
    let fake = happy_path();
    *fake.inventory.lock().unwrap() = vec![
        campaign("c1", true, vec![drop("d1", 60, 0)]),
        campaign("c-other", true, vec![drop("d-other", 60, 0)]),
    ];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    let stalled = now() + chrono::Duration::seconds(400);
    assert!(matches!(e.assess_health(stalled), Verdict::Escalate(_)));

    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d-other"),
            current_minutes: 10,
            required_minutes: 60,
        },
        stalled,
    );

    let still_stalled = stalled + chrono::Duration::seconds(120);
    assert!(
        matches!(e.assess_health(still_stalled), Verdict::Escalate(_) | Verdict::Waiting),
        "the stall must survive another campaign's progress"
    );
}
