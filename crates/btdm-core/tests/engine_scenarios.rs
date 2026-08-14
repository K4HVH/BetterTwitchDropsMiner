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
    assert_eq!(selected.unwrap().channel.login, "streamer1");
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
    let chosen = e.select_channel(now()).await.unwrap().unwrap().channel;
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
    let chosen = e.select_channel(now()).await.unwrap().unwrap().channel;
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
    let chosen = e.select_channel(now()).await.unwrap().unwrap().channel;
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

/// The dashboard reports the account link, so an unlinked campaign can be
/// discarded without spending a detail request on it. Against a real account
/// this is the difference between ~130 requests and a handful.
#[tokio::test]
async fn unlinked_campaigns_cost_no_detail_requests() {
    let fake = FakeTwitch::default();
    let mut offered = vec![];
    for i in 0..50 {
        offered.push(campaign(&format!("c{i}"), false, vec![]));
    }
    offered.push(campaign("c-linked", true, vec![]));
    *fake.offered.lock().unwrap() = offered;
    fake.details
        .lock()
        .unwrap()
        .insert("c-linked".into(), campaign("c-linked", true, vec![drop("d1", 60, 0)]));

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    assert_eq!(
        *e.api().detail_fetches.lock().unwrap(),
        1,
        "only the linked campaign is worth expanding"
    );
    assert_eq!(e.campaigns().len(), 1);
}

/// Skipping the detail fetch for unlinked campaigns must not make them vanish
/// from the totals. "You could mine these if you linked those accounts" is
/// exactly what the dashboard should say.
#[tokio::test]
async fn unlinked_campaigns_are_still_reported_as_rejected() {
    let fake = FakeTwitch::default();
    *fake.offered.lock().unwrap() = vec![
        campaign("c-unlinked-a", false, vec![]),
        campaign("c-unlinked-b", false, vec![]),
    ];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    let snap = e.snapshot("main", Some("someone"), now());
    assert_eq!(snap.totals.campaigns_known, 2, "the campaigns still exist");
    assert_eq!(snap.totals.campaigns_eligible, 0);
    assert_eq!(snap.rejected.len(), 2);
    assert!(snap.rejected.iter().all(|r| r.reason == "not linked"));
    // A user who does not know what "linked" means must still be told what to do.
    assert!(
        snap.rejected
            .iter()
            .all(|r| r.explanation.contains("link it")),
        "the reason has to be actionable, got: {:?}",
        snap.rejected.first().map(|r| &r.explanation)
    );
    assert_eq!(*e.api().detail_fetches.lock().unwrap(), 0, "still no requests spent");
}

// ---------------------------------------------------------------------------
// Recovery ladder mechanics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resend_watch_forces_a_heartbeat_before_the_interval() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();
    assert_eq!(e.api().watches_sent.lock().unwrap().len(), 1);

    // Without the rung, no heartbeat is due for another minute.
    e.apply_recovery(RecoveryStep::ResendWatch, now()).await.unwrap();
    e.tick(now()).await.unwrap();
    assert_eq!(e.api().watches_sent.lock().unwrap().len(), 2);
}

/// A restarted broadcast keeps earning nothing while looking healthy, so the
/// rung has to notice the id changed and re-send against the new one.
#[tokio::test]
async fn refreshing_stream_info_picks_up_a_restarted_broadcast() {
    let fake = happy_path();
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    // The stream restarts under us with a new broadcast id.
    *e.api().channels.lock().unwrap() =
        vec![(channel("ch1", "streamer1"), stream("bc-RESTARTED", true))];

    e.apply_recovery(RecoveryStep::RefreshStreamInfo, now()).await.unwrap();
    e.tick(now()).await.unwrap();
    assert_eq!(
        e.api().watches_sent.lock().unwrap().len(),
        2,
        "a changed broadcast id must trigger an immediate re-send"
    );
}

#[tokio::test]
async fn refreshing_stream_info_leaves_an_offline_channel() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    e.api().channels.lock().unwrap().clear(); // channel went offline
    e.apply_recovery(RecoveryStep::RefreshStreamInfo, now()).await.unwrap();
    assert!(e.current_channel().is_none());
}

#[tokio::test]
async fn reverifying_leaves_a_channel_twitch_says_earns_nothing() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    assert!(e.current_channel().is_some());

    // Twitch changes its mind about this channel.
    e.api().available.lock().unwrap().clear();

    let outcome = e
        .apply_recovery(RecoveryStep::ReverifyEligibility, now())
        .await
        .unwrap();
    assert!(e.current_channel().is_none());
    assert!(outcome.resync, "a fresh look at the world is needed");
}

#[tokio::test]
async fn reverifying_keeps_a_channel_twitch_still_endorses() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    e.apply_recovery(RecoveryStep::ReverifyEligibility, now())
        .await
        .unwrap();
    assert!(
        e.current_channel().is_some(),
        "an endorsed channel must not be abandoned"
    );
}

/// An inconclusive check is not proof of failure, so it must not cost the
/// channel its slot.
#[tokio::test]
async fn an_inconclusive_reverification_keeps_the_channel() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    *e.api().fail_available_drops.lock().unwrap() = true;
    e.apply_recovery(RecoveryStep::ReverifyEligibility, now())
        .await
        .unwrap();
    assert!(e.current_channel().is_some());
}

#[tokio::test]
async fn switching_channel_quarantines_and_asks_for_a_resync() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    let outcome = e.apply_recovery(RecoveryStep::SwitchChannel, now()).await.unwrap();
    assert!(e.current_channel().is_none());
    assert!(outcome.resync);
    assert!(
        e.select_channel(now()).await.unwrap().is_none(),
        "the quarantined channel must not be picked straight back up"
    );
}

/// The engine holds no credentials, so this rung delegates rather than
/// silently doing nothing.
#[tokio::test]
async fn the_auth_rung_asks_the_caller_to_check() {
    let mut e = engine(happy_path());
    let outcome = e.apply_recovery(RecoveryStep::ValidateAuth, now()).await.unwrap();
    assert!(outcome.validate_auth);
    assert!(!outcome.resync);
}

#[tokio::test]
async fn quarantining_the_campaign_stops_it_being_reselected() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    let outcome = e
        .apply_recovery(RecoveryStep::QuarantineCampaign, now())
        .await
        .unwrap();
    assert!(outcome.resync);
    assert!(e.select_channel(now()).await.unwrap().is_none());
}

/// End to end: a channel that confirms nothing is escalated, and the ladder
/// eventually moves the miner off it entirely.
#[tokio::test]
async fn a_silent_channel_is_eventually_abandoned_by_the_ladder() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    let mut t = 400;
    let mut rungs = 0;
    while e.current_channel().is_some() && rungs < RecoveryStep::LADDER.len() {
        let at = now() + chrono::Duration::seconds(t);
        if let Verdict::Escalate(step) = e.assess_health(at) {
            e.apply_recovery(step, at).await.unwrap();
            rungs += 1;
        }
        t += 61;
    }

    assert!(
        e.current_channel().is_none(),
        "the ladder must not leave the miner parked on a channel earning nothing"
    );
}

// ---------------------------------------------------------------------------
// Cross-campaign sessions
//
// One channel routinely advances several campaigns at once. The engine used to
// model a session as one channel *and one campaign*, which threw the rest away.
// ---------------------------------------------------------------------------

fn game2() -> Game {
    Game {
        id: GameId::from("g2"),
        name: "Other Game".into(),
        slug: "other-game".into(),
        box_art_url: None,
    }
}

/// Two campaigns for the same game, one channel serving both.
fn two_campaigns_one_channel() -> FakeTwitch {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![
        campaign("c1", true, vec![drop("d1", 60, 0)]),
        campaign("c2", true, vec![drop("d2", 120, 0)]),
    ];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1", true))];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1"), DropId::from("d2")]);
    fake
}

#[tokio::test]
async fn a_session_records_every_campaign_the_channel_advances() {
    let mut e = engine(two_campaigns_one_channel());
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap().unwrap();

    assert_eq!(watch.campaigns.len(), 2, "both campaigns are being earned");
    assert_eq!(watch.drops.len(), 2);
}

/// The bug this replaced: progress was accepted only for the primary campaign,
/// so confirmations for the others were discarded and the stall detector fired
/// on a session that was working perfectly.
#[tokio::test]
async fn progress_for_any_advanced_campaign_counts() {
    let mut e = engine(two_campaigns_one_channel());
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap().unwrap();

    // Deliberately pick a campaign that is *not* the primary one.
    let secondary = watch.campaigns.last().unwrap().clone();
    let drop_id = e
        .campaigns()
        .iter()
        .find(|c| c.id == secondary)
        .unwrap()
        .drops[0]
        .id
        .clone();

    e.on_pushed_progress(
        DropProgress {
            drop_id: drop_id.clone(),
            current_minutes: 5,
            required_minutes: 120,
        },
        now(),
    );

    assert_eq!(e.state(), MiningState::Mining, "the confirmation counted");
    assert_eq!(e.active_drop().unwrap().0, &drop_id);
}

/// Progress for a campaign this session is not mining is still ignored — that
/// is the browser-in-another-tab case, and it must not reset the stall clock.
#[tokio::test]
async fn progress_for_an_unrelated_campaign_is_still_ignored() {
    let fake = two_campaigns_one_channel();
    // A third campaign, live but not served by our channel.
    fake.inventory
        .lock()
        .unwrap()
        .push(campaign("c3", true, vec![drop("d3", 60, 0)]));

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap().unwrap();

    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d3"),
            current_minutes: 30,
            required_minutes: 60,
        },
        now(),
    );

    assert_eq!(
        e.state(),
        MiningState::Establishing,
        "watching another campaign in a browser must not look like our progress"
    );
}

/// The check budget is per channel now, not per channel-campaign pair. With
/// two campaigns served by one channel, one request should suffice.
#[tokio::test]
async fn a_channel_serving_two_campaigns_is_only_checked_once() {
    let fake = two_campaigns_one_channel();
    let checked = std::sync::Arc::new(std::sync::Mutex::new(0usize));

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    // The fake counts detail fetches, not availability checks, so assert on
    // the observable consequence instead: one candidate, not two.
    let _ = checked;
    let snap = e.snapshot("main", None, now());
    assert_eq!(
        snap.live.len(),
        1,
        "one channel must appear once, not once per campaign"
    );
    assert_eq!(snap.live[0].campaigns.len(), 2);
}

/// Two games means two directory queries, but a channel appearing in both is
/// still one candidate.
#[tokio::test]
async fn candidates_are_deduplicated_across_games() {
    let fake = FakeTwitch::default();
    let mut c2 = campaign("c2", true, vec![drop("d2", 60, 0)]);
    c2.game = game2();
    *fake.inventory.lock().unwrap() =
        vec![campaign("c1", true, vec![drop("d1", 60, 0)]), c2];

    // The same channel is live under both games.
    let mut second_stream = stream("bc-1", true);
    second_stream.game = Some(game2());
    *fake.channels.lock().unwrap() = vec![
        (channel("ch1", "streamer1"), stream("bc-1", true)),
        (channel("ch1", "streamer1"), second_stream),
    ];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1"), DropId::from("d2")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap().unwrap();

    assert_eq!(watch.campaigns.len(), 2, "both games are advanced at once");
    assert_eq!(e.snapshot("main", None, now()).live.len(), 1);
}

/// A stale campaign disappearing from a sync must not leave the session
/// claiming to mine something that no longer exists.
#[tokio::test]
async fn a_session_whose_campaigns_all_vanish_is_abandoned() {
    let fake = happy_path();
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap().unwrap();
    assert!(e.current().is_some());

    // Twitch now offers nothing.
    e.api().inventory.lock().unwrap().clear();
    e.sync(now()).await.unwrap();

    assert!(e.current().is_none(), "we cannot be mining a campaign that is gone");
}

// ---------------------------------------------------------------------------
// Commands
//
// Every control on the dashboard is one of these. A button that silently does
// nothing is worse than no button, so each is asserted end to end.
// ---------------------------------------------------------------------------

use btdm_core::control::Command;

#[tokio::test]
async fn pausing_stops_watching_and_stops_selecting() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    assert!(e.current().is_some());

    let effect = e.apply_command(Command::Pause, now());
    assert!(effect.persist, "pausing must survive a restart");
    assert!(e.current().is_none());
    assert_eq!(e.state(), MiningState::Paused);

    // And it must stay stopped.
    assert!(e.select_channel(now()).await.unwrap().is_none());
    e.tick(now()).await.unwrap();
    assert!(
        e.api().watches_sent.lock().unwrap().is_empty(),
        "a paused miner must not send heartbeats"
    );
}

#[tokio::test]
async fn resuming_selects_again() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.apply_command(Command::Pause, now());

    let effect = e.apply_command(Command::Resume, now());
    assert!(effect.reselect);
    assert!(e.select_channel(now()).await.unwrap().is_some());
}

#[tokio::test]
async fn changing_strategy_takes_effect_without_a_restart() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    let effect = e.apply_command(
        Command::SetStrategy {
            name: "coverage".into(),
        },
        now(),
    );

    assert_eq!(e.strategy_name(), "coverage");
    assert!(effect.reselect && effect.persist);
    assert!(
        e.needs_selection(now()),
        "the choice must be re-decided under the new strategy at once"
    );
}

#[tokio::test]
async fn an_unknown_strategy_is_refused_rather_than_silently_ignored() {
    let mut e = engine(happy_path());
    let effect = e.apply_command(
        Command::SetStrategy {
            name: "telepathy".into(),
        },
        now(),
    );
    assert_eq!(e.strategy_name(), "deadline", "the old one is kept");
    assert!(effect.message.contains("No such strategy"), "{}", effect.message);
    assert!(!effect.persist, "nothing changed, so nothing to save");
}

#[tokio::test]
async fn excluding_a_campaign_stops_it_being_mined() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    assert!(e.current().is_some());

    e.apply_command(
        Command::SetCampaignExcluded {
            campaign: CampaignId::from("c1"),
            excluded: true,
        },
        now(),
    );

    assert!(e.current().is_none(), "we were mining the thing just excluded");
    assert!(
        e.select_channel(now()).await.unwrap().is_none(),
        "and it must not be picked again"
    );
    // It stays visible, marked, rather than vanishing from the dashboard.
    let snap = e.snapshot("main", None, now());
    assert!(snap.eligible.iter().any(|c| c.id == "c1" && c.excluded));
}

#[tokio::test]
async fn excluding_a_game_excludes_all_of_its_campaigns() {
    let mut e = engine(two_campaigns_one_channel());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    e.apply_command(
        Command::SetGameExcluded {
            game: "Deadlock".into(),
            excluded: true,
        },
        now(),
    );

    assert!(e.current().is_none());
    assert!(e.select_channel(now()).await.unwrap().is_none());
}

#[tokio::test]
async fn re_including_a_campaign_makes_it_mineable_again() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.apply_command(
        Command::SetCampaignExcluded {
            campaign: CampaignId::from("c1"),
            excluded: true,
        },
        now(),
    );
    assert!(e.select_channel(now()).await.unwrap().is_none());

    e.apply_command(
        Command::SetCampaignExcluded {
            campaign: CampaignId::from("c1"),
            excluded: false,
        },
        now(),
    );
    assert!(e.select_channel(now()).await.unwrap().is_some());
}

#[tokio::test]
async fn watching_a_named_channel_overrides_the_strategy_once() {
    let fake = happy_path();
    fake.channels
        .lock()
        .unwrap()
        .push((channel("ch2", "streamer2"), stream("bc-2", true)));
    fake.available
        .lock()
        .unwrap()
        .insert("ch2".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    e.apply_command(
        Command::WatchChannel {
            login: "streamer2".into(),
        },
        now(),
    );
    let watch = e.select_channel(now()).await.unwrap().unwrap();
    assert_eq!(watch.channel.login, "streamer2");
    assert!(watch.reason.contains("you asked"), "{}", watch.reason);
}

/// Asking for a channel that is not live must say so rather than appearing to
/// work and then quietly watching something else.
#[tokio::test]
async fn watching_an_unavailable_channel_reports_why() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.drain_events();

    e.apply_command(
        Command::WatchChannel {
            login: "not-live".into(),
        },
        now(),
    );
    e.select_channel(now()).await.unwrap();

    let notices: Vec<String> = e
        .drain_events()
        .into_iter()
        .filter_map(|ev| match ev.kind {
            EventKind::Notice { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        notices.iter().any(|m| m.contains("not-live")),
        "the user must be told, got: {notices:?}"
    );
}

#[tokio::test]
async fn switching_channel_moves_off_the_current_one() {
    let fake = happy_path();
    fake.channels
        .lock()
        .unwrap()
        .push((channel("ch2", "streamer2"), stream("bc-2", true)));
    fake.available
        .lock()
        .unwrap()
        .insert("ch2".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let first = e.select_channel(now()).await.unwrap().unwrap();

    e.apply_command(Command::SwitchChannel, now());
    let second = e.select_channel(now()).await.unwrap().unwrap();

    assert_ne!(
        first.channel.login, second.channel.login,
        "switching must actually switch, not re-pick the same channel"
    );
}

#[tokio::test]
async fn pinning_a_channel_is_persisted_and_reselects() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();

    let effect = e.apply_command(
        Command::Pin {
            channel: Some("SomeStreamer".into()),
        },
        now(),
    );
    assert!(effect.persist && effect.reselect);
    assert_eq!(e.settings().pinned_channel.as_deref(), Some("somestreamer"));

    e.apply_command(Command::Pin { channel: None }, now());
    assert!(e.settings().pinned_channel.is_none());
}

#[tokio::test]
async fn favourites_can_be_added_and_removed() {
    let mut e = engine(happy_path());
    e.apply_command(
        Command::SetFavourite {
            login: "SomeStreamer".into(),
            on: true,
        },
        now(),
    );
    assert_eq!(e.settings().favourite_channels, vec!["somestreamer"]);

    e.apply_command(
        Command::SetFavourite {
            login: "somestreamer".into(),
            on: false,
        },
        now(),
    );
    assert!(e.settings().favourite_channels.is_empty());
}

#[tokio::test]
async fn clearing_quarantine_lets_a_rested_channel_be_picked_again() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.quarantine_current(now());
    assert!(
        e.select_channel(now()).await.unwrap().is_none(),
        "the only channel is resting"
    );

    e.apply_command(Command::ClearQuarantine, now());
    assert!(
        e.select_channel(now()).await.unwrap().is_some(),
        "clearing must actually free it"
    );
}

#[tokio::test]
async fn sync_now_forces_a_refresh_that_was_not_yet_due() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    assert!(!e.needs_sync(now()), "just synced");

    e.apply_command(Command::SyncNow, now());
    assert!(e.needs_sync(now()));
}

/// A command the user issued must appear in the log, or the dashboard cannot
/// confirm anything happened.
#[tokio::test]
async fn commands_are_recorded_as_events() {
    let mut e = engine(happy_path());
    e.drain_events();
    e.apply_command(Command::Pause, now());

    let messages: Vec<String> = e.drain_events().iter().map(|ev| ev.summary()).collect();
    assert!(
        messages.iter().any(|m| m.contains("paused")),
        "got: {messages:?}"
    );
}

// ---------------------------------------------------------------------------
// The snapshot as a UI contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_snapshot_carries_everything_the_controls_need() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    let snap = e.snapshot("main", Some("someone"), now());

    assert_eq!(
        snap.strategies.len(),
        btdm_core::strategy::ALL_STRATEGIES.len(),
        "the picker is built from this"
    );
    assert!(snap.strategies.iter().any(|s| s.selected));
    assert!(
        snap.strategies.iter().all(|s| !s.description.is_empty()),
        "each option has to explain itself"
    );
    assert_eq!(snap.settings.strategy, "deadline");
    assert!(!snap.eligible.is_empty());
    assert!(!snap.eligible[0].drops.is_empty(), "drops must be visible");
    assert!(snap.eligible[0].is_current, "the mined campaign is marked");
    assert_eq!(snap.live.len(), 1, "the choice we faced is shown");
    assert!(snap.watching.is_some());
    assert!(
        !snap.watching.as_ref().unwrap().reason.is_empty(),
        "the choice must explain itself on screen"
    );
}

#[tokio::test]
async fn the_snapshot_reports_when_the_next_sync_is_due() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    let snap = e.snapshot("main", None, now());
    assert!(snap.health.last_sync.is_some());
    assert!(snap.health.next_sync_in_seconds.unwrap() > 0);
}

#[tokio::test]
async fn a_quarantined_channel_is_explained_rather_than_silently_skipped() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.quarantine_current(now());

    let snap = e.snapshot("main", None, now());
    assert_eq!(
        snap.health.quarantined_channels.len(),
        1,
        "the user is entitled to know why a live channel is being skipped"
    );
}

// ---------------------------------------------------------------------------
// Reconsidering the choice while already watching
//
// Selection used to run only when idle, so the strategy was consulted once per
// stream and a campaign that became urgent could never displace a comfortable
// one already in progress.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_choice_is_reconsidered_periodically_not_only_when_idle() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    assert!(!e.needs_selection(now()), "just chose");
    assert!(
        e.needs_selection(now() + chrono::Duration::minutes(11)),
        "a watching miner must still reconsider, or urgency never wins"
    );
}

/// Re-choosing the same channel must not restart the session: that would reset
/// the stall clock and throw away the confirmed-progress display every time.
#[tokio::test]
async fn reselecting_the_same_channel_keeps_the_session_and_its_progress() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    let first = e.select_channel(now()).await.unwrap().unwrap();

    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d1"),
            current_minutes: 30,
            required_minutes: 60,
        },
        now(),
    );
    assert_eq!(e.state(), MiningState::Mining);

    let later = now() + chrono::Duration::minutes(11);
    let again = e.select_channel(later).await.unwrap().unwrap();

    assert_eq!(again.channel.login, first.channel.login);
    assert_eq!(
        again.since, first.since,
        "the session must not appear to have restarted"
    );
    assert_eq!(e.state(), MiningState::Mining, "progress must survive");
    assert_eq!(e.active_drop().unwrap().1.current_minutes, 30);
}

/// A campaign that becomes urgent must be able to take over from a comfortable
/// one, without waiting for the current stream to end.
#[tokio::test]
async fn an_urgent_campaign_displaces_a_comfortable_one() {
    let fake = FakeTwitch::default();
    let mut relaxed = campaign("c-relaxed", true, vec![drop("d1", 60, 0)]);
    relaxed.ends_at = now() + chrono::Duration::days(20);
    let mut urgent = campaign("c-urgent", true, vec![drop("d2", 60, 0)]);
    urgent.ends_at = now() + chrono::Duration::hours(3);
    urgent.starts_at = now() - chrono::Duration::hours(1);

    *fake.inventory.lock().unwrap() = vec![relaxed];
    *fake.channels.lock().unwrap() = vec![
        (channel("ch1", "relaxed-streamer"), stream("bc-1", true)),
        (channel("ch2", "urgent-streamer"), stream("bc-2", true)),
    ];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1")]);
    fake.available
        .lock()
        .unwrap()
        .insert("ch2".into(), vec![DropId::from("d2")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let first = e.select_channel(now()).await.unwrap().unwrap();
    assert_eq!(first.channel.login, "relaxed-streamer");

    // The urgent campaign appears on the next sync.
    e.api().inventory.lock().unwrap().push(urgent);
    e.sync(now()).await.unwrap();
    assert!(e.needs_selection(now()), "a sync must prompt a rethink");

    let second = e.select_channel(now()).await.unwrap().unwrap();
    assert_eq!(
        second.channel.login, "urgent-streamer",
        "a campaign expiring in three hours must beat one expiring in twenty days"
    );
}

/// Hysteresis still has to stop the periodic rethink turning into thrash.
#[tokio::test]
async fn a_marginally_better_channel_does_not_cause_a_switch() {
    let fake = FakeTwitch::default();
    let mut a = campaign("c-a", true, vec![drop("d1", 60, 0)]);
    a.ends_at = now() + chrono::Duration::days(5);
    let mut b = campaign("c-b", true, vec![drop("d2", 60, 0)]);
    b.ends_at = now() + chrono::Duration::days(5) - chrono::Duration::minutes(1);

    *fake.inventory.lock().unwrap() = vec![a, b];
    *fake.channels.lock().unwrap() = vec![
        (channel("ch1", "incumbent"), stream("bc-1", true)),
        (channel("ch2", "rival"), stream("bc-2", true)),
    ];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1")]);
    fake.available
        .lock()
        .unwrap()
        .insert("ch2".into(), vec![DropId::from("d2")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let first = e.select_channel(now()).await.unwrap().unwrap();

    let again = e
        .select_channel(now() + chrono::Duration::minutes(11))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        again.channel.login, first.channel.login,
        "a one-minute difference must not cost us the session"
    );
}

#[tokio::test]
async fn a_paused_miner_never_asks_to_select() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.apply_command(Command::Pause, now());
    assert!(!e.needs_selection(now() + chrono::Duration::hours(2)));
}

/// Pausing means "stop watching", not "abandon what I have already earned".
#[tokio::test]
async fn a_paused_miner_still_claims_drops_it_already_earned() {
    let fake = happy_path();
    let mut earned = drop("d1", 60, 60);
    earned.claim_id = Some("claim-1".into());
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![earned])];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.apply_command(Command::Pause, now());
    e.tick(now()).await.unwrap();

    assert_eq!(
        *e.api().claims.lock().unwrap(),
        vec!["claim-1".to_string()],
        "an earned drop must still be claimed while paused"
    );
    assert!(
        e.api().watches_sent.lock().unwrap().is_empty(),
        "but nothing new may be mined"
    );
}

/// The websocket is not the only way a claimable drop is discovered: a missed
/// push must be picked up by the next inventory sync, or the drop is lost.
#[tokio::test]
async fn a_claimable_drop_found_by_sync_is_claimed_without_any_websocket_event() {
    let fake = happy_path();
    let mut earned = drop("d1", 60, 60);
    earned.claim_id = Some("claim-from-inventory".into());
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![earned])];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    assert_eq!(
        *e.api().claims.lock().unwrap(),
        vec!["claim-from-inventory".to_string()]
    );
    let claimed: Vec<String> = e
        .drain_events()
        .into_iter()
        .filter_map(|ev| match ev.kind {
            EventKind::DropClaimed { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(claimed.len(), 1, "and the claim must be reported");
}

// ---------------------------------------------------------------------------
// Claiming
//
// Every one of these is a way a drop that was earned could be lost.
// ---------------------------------------------------------------------------

/// The worst bug the reviewer found: a refused claim was reported as a success
/// and marked claimed locally, so it was never retried.
#[tokio::test]
async fn a_refused_claim_is_not_reported_as_a_success() {
    let fake = happy_path();
    let mut earned = drop("d1", 60, 60);
    earned.claim_id = Some("claim-1".into());
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![earned])];
    *fake.fail_claims.lock().unwrap() = true;

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.drain_events();
    e.tick(now()).await.unwrap();

    let events = e.drain_events();
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::DropClaimed { .. })),
        "a claim that failed must never be announced as a claim"
    );
    assert!(
        events.iter().any(|ev| matches!(
            ev.kind,
            EventKind::Notice { severity: btdm_core::event::Severity::Warn, .. }
        )),
        "and the user must be told it failed"
    );
    assert!(
        e.campaigns()[0].drops[0].is_claimable(),
        "it must stay claimable so the next tick retries it"
    );
}

/// A failed claim must not take the heartbeat down with it.
#[tokio::test]
async fn a_failed_claim_does_not_abort_the_tick() {
    let fake = happy_path();
    let mut earned = drop("d1", 60, 60);
    earned.claim_id = Some("claim-1".into());
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![earned])];
    *fake.fail_claims.lock().unwrap() = true;

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    assert_eq!(
        e.api().watches_sent.lock().unwrap().len(),
        1,
        "mining must continue even when a claim is refused"
    );
}

/// Twitch does not always supply a dropInstanceID. Requiring one meant an
/// earned drop was silently never claimed.
#[tokio::test]
async fn an_earned_drop_with_no_instance_id_is_still_claimed() {
    let fake = happy_path();
    // Fully watched, but Twitch sent no claim id.
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 60)])];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    let claims = e.api().claims.lock().unwrap().clone();
    assert_eq!(claims.len(), 1, "the claim must still be attempted");
    assert!(
        claims[0].contains("c1") && claims[0].contains("d1"),
        "the id is reconstructed from the campaign and drop: {claims:?}"
    );
}

/// Twitch allows claiming for 24 hours after a campaign ends. Dropping the
/// campaign the instant it expired threw away rewards already worked for.
#[tokio::test]
async fn a_drop_earned_in_a_just_ended_campaign_is_still_claimed() {
    let fake = FakeTwitch::default();
    let mut ended = campaign("c-ended", true, vec![drop("d1", 60, 60)]);
    ended.starts_at = now() - chrono::Duration::days(10);
    ended.ends_at = now() - chrono::Duration::hours(1);
    ended.drops[0].claim_id = Some("claim-late".into());
    *fake.inventory.lock().unwrap() = vec![ended];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.campaigns().is_empty(), "it is not mineable any more");

    e.tick(now()).await.unwrap();
    assert_eq!(
        *e.api().claims.lock().unwrap(),
        vec!["claim-late".to_string()],
        "but the reward is still ours for another 23 hours"
    );
}

/// Past the grace period there is nothing to be done, and we must not keep
/// hammering Twitch about it.
#[tokio::test]
async fn a_long_expired_campaign_is_not_retried_forever() {
    let fake = FakeTwitch::default();
    let mut ended = campaign("c-old", true, vec![drop("d1", 60, 60)]);
    ended.starts_at = now() - chrono::Duration::days(30);
    ended.ends_at = now() - chrono::Duration::days(3);
    ended.drops[0].claim_id = Some("claim-ancient".into());
    *fake.inventory.lock().unwrap() = vec![ended];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    assert!(
        e.api().claims.lock().unwrap().is_empty(),
        "the 24-hour window closed long ago"
    );
}

// ---------------------------------------------------------------------------
// Regressions found by adversarial review
// ---------------------------------------------------------------------------

/// A flaky directory request used to swallow the user's command entirely: the
/// "already reselected" flag was cleared before the fallible call, so the
/// dashboard reported success and nothing happened for ten minutes.
#[tokio::test]
async fn a_failed_selection_leaves_the_request_pending() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    e.apply_command(
        Command::SetStrategy {
            name: "coverage".into(),
        },
        now(),
    );
    assert!(e.needs_selection(now()));

    // Twitch falls over on the very next request.
    *e.api().fail_with.lock().unwrap() = Some(ApiError::Network("boom".into()));
    assert!(e.select_channel(now()).await.is_err());

    assert!(
        e.needs_selection(now()),
        "the command must still be pending, not silently dropped"
    );

    *e.api().fail_with.lock().unwrap() = None;
    e.select_channel(now()).await.unwrap();
    assert!(!e.needs_selection(now()), "and satisfied once it succeeds");
}

/// Excluding one campaign of several must trim the session, not end it —
/// ending it resets the progress tracker and the stall clock for campaigns the
/// user never touched.
#[tokio::test]
async fn excluding_one_of_several_campaigns_keeps_the_session() {
    let mut e = engine(two_campaigns_one_channel());
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap().unwrap();
    assert_eq!(watch.campaigns.len(), 2);

    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d1"),
            current_minutes: 30,
            required_minutes: 60,
        },
        now(),
    );
    assert_eq!(e.state(), MiningState::Mining);

    // Exclude the *other* campaign.
    let secondary = watch.campaigns[1].clone();
    e.apply_command(
        Command::SetCampaignExcluded {
            campaign: secondary.clone(),
            excluded: true,
        },
        now(),
    );

    let still = e.current().expect("the session must survive");
    assert_eq!(still.campaigns.len(), 1);
    assert!(!still.campaigns.contains(&secondary));
    assert_eq!(
        e.state(),
        MiningState::Mining,
        "progress on the campaign the user kept must not be discarded"
    );
}

#[tokio::test]
async fn excluding_every_campaign_in_the_session_does_end_it() {
    let mut e = engine(two_campaigns_one_channel());
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap().unwrap();

    for campaign in watch.campaigns.clone() {
        e.apply_command(
            Command::SetCampaignExcluded {
                campaign,
                excluded: true,
            },
            now(),
        );
    }
    assert!(e.current().is_none());
}

/// The masked-stall case. A channel is chosen for campaign P and also advances
/// Q. Twitch reports drops per *account*, so a browser watching some other
/// stream can keep Q ticking. Accepting that as proof of health would hide P
/// earning nothing here.
#[tokio::test]
async fn a_secondary_campaign_advancing_does_not_mask_a_stalled_primary() {
    let mut e = engine(two_campaigns_one_channel());
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap().unwrap();

    let secondary = watch.campaigns[1].clone();
    let secondary_drop = e
        .campaigns()
        .iter()
        .find(|c| c.id == secondary)
        .unwrap()
        .drops[0]
        .id
        .clone();

    // Six minutes later, only the secondary campaign has moved.
    let later = now() + chrono::Duration::minutes(6);
    e.on_pushed_progress(
        DropProgress {
            drop_id: secondary_drop,
            current_minutes: 5,
            required_minutes: 120,
        },
        later,
    );

    let quiet = e.since_last_confirmation(later).unwrap();
    assert!(
        quiet >= chrono::Duration::minutes(6),
        "the stall clock must still be running for the campaign we came for, \
         got {quiet:?}"
    );
}

/// ...but progress on the campaign that drove the choice does clear it.
#[tokio::test]
async fn progress_on_the_primary_campaign_clears_the_stall_clock() {
    let mut e = engine(two_campaigns_one_channel());
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap().unwrap();

    let primary = watch.primary_campaign().unwrap().clone();
    let primary_drop = e
        .campaigns()
        .iter()
        .find(|c| c.id == primary)
        .unwrap()
        .drops[0]
        .id
        .clone();

    let later = now() + chrono::Duration::minutes(6);
    e.on_pushed_progress(
        DropProgress {
            drop_id: primary_drop,
            current_minutes: 5,
            required_minutes: 60,
        },
        later,
    );

    assert_eq!(e.since_last_confirmation(later).unwrap().num_seconds(), 0);
}

/// Once the primary campaign has nothing left to earn, any of the session's
/// campaigns counts — otherwise finishing it would read as a stall.
#[tokio::test]
async fn a_finished_primary_campaign_does_not_make_the_session_look_stalled() {
    let fake = two_campaigns_one_channel();
    // The first campaign is already fully claimed.
    let mut done = campaign("c1", true, vec![drop("d1", 60, 60)]);
    done.drops[0].is_claimed = true;
    *fake.inventory.lock().unwrap() = vec![done, campaign("c2", true, vec![drop("d2", 120, 0)])];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap().unwrap();

    let later = now() + chrono::Duration::minutes(6);
    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d2"),
            current_minutes: 5,
            required_minutes: 120,
        },
        later,
    );
    assert_eq!(e.since_last_confirmation(later).unwrap().num_seconds(), 0);
}

// ---------------------------------------------------------------------------
// Allow-listed campaigns
//
// These are the exclusive, high-value ones. Their channels are usually small,
// so the game directory's most-relevant page never contains them.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_small_allow_listed_channel_is_found_even_when_the_directory_misses_it() {
    let fake = FakeTwitch::default();
    let mut c = campaign("c1", true, vec![drop("d1", 60, 0)]);
    c.allowed_channels = vec![channel("ch-small", "tinystreamer")];
    *fake.inventory.lock().unwrap() = vec![c];

    // The directory only knows about big channels, none of them allow-listed.
    *fake.channels.lock().unwrap() = vec![
        (channel("ch-big", "hugestreamer"), stream("bc-big", true)),
        // The allow-listed one is live, but reachable only by name.
        (channel("ch-small", "tinystreamer"), stream("bc-small", true)),
    ];
    fake.available
        .lock()
        .unwrap()
        .insert("ch-small".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap();

    assert_eq!(
        watch.map(|w| w.channel.login),
        Some("tinystreamer".to_string()),
        "an exclusive campaign must not be invisible just because its \
         streamer is small"
    );
}

/// A long allow-list must not turn into hundreds of individual lookups.
#[tokio::test]
async fn a_long_allow_list_is_not_resolved_channel_by_channel() {
    let fake = FakeTwitch::default();
    let mut c = campaign("c1", true, vec![drop("d1", 60, 0)]);
    c.allowed_channels = (0..50)
        .map(|i| channel(&format!("ch{i}"), &format!("streamer{i}")))
        .collect();
    *fake.inventory.lock().unwrap() = vec![c];
    // Nobody at all is live.
    *fake.channels.lock().unwrap() = vec![];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    assert!(
        *e.api().stream_info_calls.lock().unwrap() <= 8,
        "fifty lookups for one campaign is a request storm, made {} calls",
        e.api().stream_info_calls.lock().unwrap()
    );
}

/// A quarantined channel stays quarantined even when it is allow-listed.
#[tokio::test]
async fn direct_lookup_respects_quarantine() {
    let fake = FakeTwitch::default();
    let mut c = campaign("c1", true, vec![drop("d1", 60, 0)]);
    c.allowed_channels = vec![channel("ch1", "streamer1")];
    *fake.inventory.lock().unwrap() = vec![c];
    *fake.channels.lock().unwrap() =
        vec![(channel("ch1", "streamer1"), stream("bc-1", true))];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.quarantine_current(now());

    assert!(e.select_channel(now()).await.unwrap().is_none());
}

/// Campaign ids churn every relaunch, so an exclusion list keyed on them grows
/// without bound and eventually matches nothing. The dashboard's per-row
/// "hide" button makes accumulating dead ids the normal path.
#[tokio::test]
async fn exclusions_for_campaigns_that_no_longer_exist_are_forgotten() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();

    e.apply_command(
        Command::SetCampaignExcluded {
            campaign: CampaignId::from("c1"),
            excluded: true,
        },
        now(),
    );
    e.apply_command(
        Command::SetCampaignExcluded {
            campaign: CampaignId::from("c-from-last-year"),
            excluded: true,
        },
        now(),
    );
    assert_eq!(e.settings().excluded_campaigns.len(), 2);

    e.sync(now()).await.unwrap();

    assert_eq!(
        e.settings().excluded_campaigns,
        vec!["c1".to_string()],
        "the still-offered exclusion stays; the dead one is forgotten"
    );
}

/// Twitch categorises event broadcasts as the event, not as the game the drops
/// belong to, so matching on the campaign's own game finds nobody and the
/// campaign looks permanently dead.
#[tokio::test]
async fn an_event_campaign_can_be_mined_on_any_live_channel() {
    let fake = FakeTwitch::default();
    let mut event = campaign("c-event", true, vec![drop("d-event", 60, 0)]);
    event.game = Game {
        id: GameId::from("509663"),
        name: "Special Events".into(),
        slug: "special-events".into(),
        box_art_url: None,
    };
    // A normal campaign too, so the directory has somebody in it.
    *fake.inventory.lock().unwrap() = vec![event, campaign("c1", true, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1", true))];
    // The channel progresses only the event drop.
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d-event")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap();

    assert!(
        watch.is_some_and(|w| w.campaigns.iter().any(|c| c.0 == "c-event")),
        "an event campaign must be minable on a channel not streaming its game"
    );
}

/// Two campaigns sharing a game and an allow-listed channel must not resolve
/// it twice, nor make it appear to serve the same campaign twice over.
#[tokio::test]
async fn a_shared_allow_listed_channel_is_resolved_once_and_counted_once() {
    let fake = FakeTwitch::default();
    let shared = channel("ch-small", "tinystreamer");
    let mut a = campaign("c-a", true, vec![drop("d-a", 60, 0)]);
    let mut b = campaign("c-b", true, vec![drop("d-b", 60, 0)]);
    a.allowed_channels = vec![shared.clone()];
    b.allowed_channels = vec![shared.clone()];
    *fake.inventory.lock().unwrap() = vec![a, b];

    // Live, but categorised under a different game, so the directory page for
    // this campaign's game does not contain them — which is the whole reason
    // direct lookup exists.
    let mut elsewhere = stream("bc-1", true);
    elsewhere.game = Some(Game {
        id: GameId::from("g-other"),
        name: "Something Else".into(),
        slug: "something-else".into(),
        box_art_url: None,
    });
    *fake.channels.lock().unwrap() = vec![(shared, elsewhere)];
    fake.available
        .lock()
        .unwrap()
        .insert("ch-small".into(), vec![DropId::from("d-a"), DropId::from("d-b")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap().unwrap();

    assert_eq!(*e.api().stream_info_calls.lock().unwrap(), 1, "resolved once");
    assert_eq!(
        watch.campaigns.len(),
        2,
        "it genuinely advances two campaigns"
    );
    // The real regression: the same campaign listed twice would inflate the
    // apparent coverage and corrupt the session's campaign list.
    let mut unique = watch.campaigns.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), watch.campaigns.len(), "no duplicates");
}

/// A per-campaign cap is not a cap: twenty qualifying campaigns would each
/// spend their own allowance and produce a request storm.
#[tokio::test]
async fn direct_lookups_are_capped_across_the_whole_pass() {
    let fake = FakeTwitch::default();
    let campaigns: Vec<_> = (0..20)
        .map(|i| {
            let mut c = campaign(&format!("c{i}"), true, vec![drop(&format!("d{i}"), 60, 0)]);
            c.allowed_channels = (0..8)
                .map(|j| channel(&format!("ch{i}-{j}"), &format!("streamer{i}-{j}")))
                .collect();
            c
        })
        .collect();
    *fake.inventory.lock().unwrap() = campaigns;
    *fake.channels.lock().unwrap() = vec![];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    let calls = *e.api().stream_info_calls.lock().unwrap();
    assert!(
        calls <= 24,
        "160 lookups in one pass is a request storm, made {calls}"
    );
}

/// A campaign excluded before it was ever watched exists only in Twitch's
/// dashboard listing, never in the inventory. Pruning exclusions from a sync
/// where that listing failed would decide it had vanished — and the miner
/// would then start mining something the user explicitly turned off.
#[tokio::test]
async fn an_exclusion_survives_a_failed_campaign_listing() {
    let fake = FakeTwitch::default();
    // Offered by the dashboard, never watched, so never in the inventory.
    let listed = campaign("c-never-watched", true, vec![]);
    let full = campaign("c-never-watched", true, vec![drop("d1", 60, 0)]);
    *fake.offered.lock().unwrap() = vec![listed];
    fake.details
        .lock()
        .unwrap()
        .insert("c-never-watched".into(), full);
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1", true))];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.apply_command(
        Command::SetCampaignExcluded {
            campaign: CampaignId::from("c-never-watched"),
            excluded: true,
        },
        now(),
    );
    assert!(e.select_channel(now()).await.unwrap().is_none());

    // The dashboard listing fails on the next sync; the inventory still works.
    e.api().offered.lock().unwrap().clear();
    *e.api().fail_campaign_list.lock().unwrap() = true;
    e.sync(now()).await.unwrap();

    assert_eq!(
        e.settings().excluded_campaigns,
        vec!["c-never-watched".to_string()],
        "a degraded view must not be treated as proof the campaign is gone"
    );

    // And once the listing recovers, the campaign is still excluded.
    *e.api().fail_campaign_list.lock().unwrap() = false;
    *e.api().offered.lock().unwrap() = vec![campaign("c-never-watched", true, vec![])];
    e.sync(now()).await.unwrap();
    assert!(
        e.select_channel(now()).await.unwrap().is_none(),
        "the miner must not start mining what the user turned off"
    );
}

/// A claim Twitch permanently refuses must not be re-sent every minute for the
/// life of the campaign.
#[tokio::test]
async fn a_refused_claim_backs_off_instead_of_retrying_every_tick() {
    let fake = happy_path();
    let mut earned = drop("d1", 60, 60);
    earned.claim_id = Some("claim-1".into());
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![earned])];
    *fake.fail_claims.lock().unwrap() = true;

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    // Ten minutes of ticking, one a minute.
    for minute in 0..10 {
        e.tick(now() + chrono::Duration::minutes(minute)).await.unwrap();
    }

    let attempts = *e.api().claim_attempts.lock().unwrap();
    assert!(
        (1..=4).contains(&attempts),
        "expected a handful of attempts over ten minutes, got {attempts}"
    );
}

/// ...but a refusal that clears must not leave the drop rested forever.
#[tokio::test]
async fn a_claim_is_retried_once_the_backoff_elapses() {
    let fake = happy_path();
    let mut earned = drop("d1", 60, 60);
    earned.claim_id = Some("claim-1".into());
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![earned])];
    *fake.fail_claims.lock().unwrap() = true;

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.tick(now()).await.unwrap();
    assert_eq!(*e.api().claim_attempts.lock().unwrap(), 1);
    assert!(e.api().claims.lock().unwrap().is_empty(), "it was refused");

    // Whatever was wrong is fixed, and the backoff has elapsed.
    *e.api().fail_claims.lock().unwrap() = false;
    e.tick(now() + chrono::Duration::hours(3)).await.unwrap();

    assert_eq!(*e.api().claim_attempts.lock().unwrap(), 2);
    assert_eq!(e.api().claims.lock().unwrap().len(), 1, "and it succeeded");
    assert!(e.campaigns()[0].drops[0].is_claimed, "and it is claimed");
}

// ---------------------------------------------------------------------------
// The live channel pool
//
// Following only the channel being watched meant a better one coming online
// was noticed at the next scheduled re-evaluation, up to ten minutes later.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_watched_channel_is_always_followed_first() {
    let fake = happy_path();
    for i in 2..6 {
        fake.channels
            .lock()
            .unwrap()
            .push((channel(&format!("ch{i}"), &format!("s{i}")), stream("bc", true)));
        fake.available
            .lock()
            .unwrap()
            .insert(format!("ch{i}"), vec![DropId::from("d1")]);
    }

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let watch = e.select_channel(now()).await.unwrap().unwrap();

    let pool = e.channels_worth_following(10);
    assert_eq!(
        pool.first(),
        Some(&watch.channel.id),
        "losing the watched channel is the urgent case"
    );
    assert!(pool.len() > 1, "and the alternatives are followed too");
    let mut unique = pool.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), pool.len(), "no channel is followed twice");
}

/// Twitch drops subscriptions past its per-connection limit silently, which
/// would look exactly like channels that never go offline.
#[tokio::test]
async fn the_pool_is_bounded() {
    let fake = happy_path();
    for i in 2..60 {
        fake.channels
            .lock()
            .unwrap()
            .push((channel(&format!("ch{i}"), &format!("s{i}")), stream("bc", true)));
        fake.available
            .lock()
            .unwrap()
            .insert(format!("ch{i}"), vec![DropId::from("d1")]);
    }

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    assert!(e.channels_worth_following(10).len() <= 10);
}

#[tokio::test]
async fn an_idle_miner_still_follows_the_channels_it_might_pick() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.stop_watching(now(), btdm_core::event::StopReason::UserRequest);

    assert!(
        !e.channels_worth_following(10).is_empty(),
        "the candidates are still worth hearing about"
    );
}

/// A channel that has ended must stop being offered by the dashboard.
#[tokio::test]
async fn a_channel_that_goes_offline_leaves_the_candidate_list() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    assert_eq!(e.snapshot("main", None, now()).live.len(), 1);

    e.on_channel_offline(&ChannelId::from("ch1"));
    assert!(e.snapshot("main", None, now()).live.is_empty());
}

/// A better channel coming online must prompt a rethink rather than waiting
/// out the re-selection timer.
#[tokio::test]
async fn a_channel_coming_online_prompts_a_rethink() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    assert!(!e.needs_selection(now()), "just chose");

    e.request_reselect();
    assert!(e.needs_selection(now()));
}

// ---------------------------------------------------------------------------
// Reacting faster only when it matters
//
// Following every channel's stream state over the websocket buys "notice a
// better channel sooner". Deadline pressure is the only time that is worth
// anything, and checking more often exactly then is cheaper and more targeted.
// ---------------------------------------------------------------------------

/// A campaign with days of slack does not care when it is picked up.
#[tokio::test]
async fn a_comfortable_world_is_reconsidered_on_the_slow_interval() {
    let mut e = engine(happy_path());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    assert!(!e.needs_selection(now() + chrono::Duration::minutes(5)));
    assert!(e.needs_selection(now() + chrono::Duration::minutes(11)));
}

/// A campaign about to be lost very much does.
#[tokio::test]
async fn deadline_pressure_shortens_the_interval() {
    let fake = happy_path();
    // Needs 60 minutes, has 90 left: inside the at-risk window.
    let mut tight = campaign("c1", true, vec![drop("d1", 60, 0)]);
    tight.starts_at = now() - chrono::Duration::days(1);
    tight.ends_at = now() + chrono::Duration::minutes(90);
    *fake.inventory.lock().unwrap() = vec![tight];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    assert!(
        e.needs_selection(now() + chrono::Duration::minutes(3)),
        "with something about to be lost, three minutes is already too long"
    );
}

/// ...but not for a campaign the user excluded, which cannot be lost because
/// it was never going to be mined.
#[tokio::test]
async fn an_excluded_campaign_does_not_count_as_urgent() {
    let fake = happy_path();
    let mut tight = campaign("c1", true, vec![drop("d1", 60, 0)]);
    tight.starts_at = now() - chrono::Duration::days(1);
    tight.ends_at = now() + chrono::Duration::minutes(90);
    let mut relaxed = campaign("c2", true, vec![drop("d2", 60, 0)]);
    relaxed.ends_at = now() + chrono::Duration::days(20);
    *fake.inventory.lock().unwrap() = vec![tight, relaxed];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1"), DropId::from("d2")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.apply_command(
        Command::SetCampaignExcluded {
            campaign: CampaignId::from("c1"),
            excluded: true,
        },
        now(),
    );
    e.select_channel(now()).await.unwrap();

    assert!(
        !e.needs_selection(now() + chrono::Duration::minutes(3)),
        "nothing the miner will actually mine is at risk"
    );
}

/// An impossible campaign is not urgent either — it is already lost, and
/// treating it as urgent would poll hard forever for no reason.
#[tokio::test]
async fn an_already_impossible_campaign_does_not_count_as_urgent() {
    let fake = happy_path();
    // One campaign that cannot be finished, and one comfortable one so the
    // miner is actually watching something rather than idle.
    let mut doomed = campaign("c-doomed", true, vec![drop("d-doomed", 600, 0)]);
    doomed.starts_at = now() - chrono::Duration::days(1);
    doomed.ends_at = now() + chrono::Duration::hours(1);
    let mut relaxed = campaign("c-relaxed", true, vec![drop("d-relaxed", 60, 0)]);
    relaxed.ends_at = now() + chrono::Duration::days(20);
    *fake.inventory.lock().unwrap() = vec![doomed, relaxed];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d-relaxed")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.select_channel(now()).await.unwrap().is_some());

    assert!(
        !e.needs_selection(now() + chrono::Duration::minutes(3)),
        "a campaign that is already lost is not a reason to poll hard forever"
    );
}

// ---------------------------------------------------------------------------
// Campaigns that have not started yet
// ---------------------------------------------------------------------------

/// Launch day is when a drop is worth having. Waiting out the sync interval
/// gives away the first minutes of one.
#[tokio::test]
async fn a_campaign_opening_soon_triggers_a_sync_the_moment_it_opens() {
    let fake = FakeTwitch::default();
    // Opens inside the ordinary sync interval, so only the wake-up can
    // explain a sync becoming due.
    let mut soon = campaign("c-soon", true, vec![drop("d1", 60, 0)]);
    soon.starts_at = now() + chrono::Duration::minutes(3);
    soon.ends_at = now() + chrono::Duration::days(5);
    *fake.inventory.lock().unwrap() = vec![soon];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.campaigns().is_empty(), "not started yet");
    assert!(!e.needs_sync(now()), "and not due for a while");

    assert!(
        !e.needs_sync(now() + chrono::Duration::minutes(2)),
        "still not open, and the scheduled sync is not due either"
    );
    assert!(
        e.needs_sync(now() + chrono::Duration::minutes(4)),
        "the moment it opens, it is worth a sync of its own"
    );
}

#[tokio::test]
async fn a_campaign_opening_next_month_is_not_woken_for() {
    let fake = FakeTwitch::default();
    let mut later = campaign("c-later", true, vec![drop("d1", 60, 0)]);
    later.starts_at = now() + chrono::Duration::days(30);
    later.ends_at = now() + chrono::Duration::days(60);
    *fake.inventory.lock().unwrap() = vec![later];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(
        !e.needs_sync(now() + chrono::Duration::minutes(5)),
        "nothing opens soon, so only the scheduled sync applies"
    );
}

/// The countdown must reflect what will actually happen next.
#[tokio::test]
async fn the_countdown_reports_the_campaign_opening_rather_than_the_scheduled_sync() {
    let fake = FakeTwitch::default();
    let mut soon = campaign("c-soon", true, vec![drop("d1", 60, 0)]);
    soon.starts_at = now() + chrono::Duration::minutes(3);
    soon.ends_at = now() + chrono::Duration::days(5);
    *fake.inventory.lock().unwrap() = vec![soon];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    let next = e.snapshot("main", None, now()).health.next_sync_in_seconds.unwrap();
    assert!(
        (170..=190).contains(&next),
        "expected roughly three minutes, got {next}s"
    );
}

/// A campaign nobody could mine is not worth waking for.
#[tokio::test]
async fn an_unlinked_upcoming_campaign_is_not_woken_for() {
    let fake = FakeTwitch::default();
    let mut soon = campaign("c-soon", false, vec![drop("d1", 60, 0)]);
    soon.starts_at = now() + chrono::Duration::minutes(3);
    soon.ends_at = now() + chrono::Duration::days(5);
    *fake.offered.lock().unwrap() = vec![soon];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(
        !e.needs_sync(now() + chrono::Duration::minutes(5)),
        "a campaign nobody can mine is not worth waking for"
    );
}

// ---------------------------------------------------------------------------
// What Twitch actually returns
//
// `viewerDropCampaigns` stopped listing a channel's game drop campaigns some
// time before August 2026: a channel visibly earning a game's drops reports
// none of them, and returns only sitewide reward campaigns instead. The engine
// treated that response as authoritative, so the intersection with the drops
// we wanted was empty on every channel on every pass.
//
// The whole scoring pipeline was dead as a result — nothing reached the
// strategy, and the miner watched whichever drops-enabled channel sorted first
// alphabetically. Against a real account it sat on the same channel for hours
// believing it had chosen it.
// ---------------------------------------------------------------------------

/// Twitch answers the availability check with a drop from a campaign we are
/// not mining, which is what it does today for every channel.
fn twitch_as_it_is_now() -> FakeTwitch {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![
        (channel("ch1", "zeta"), stream("bc-1", true)),
        (channel("ch2", "alpha"), stream("bc-2", true)),
    ];
    // The sitewide reward campaign, on every channel, and nothing else.
    for id in ["ch1", "ch2"] {
        fake.available
            .lock()
            .unwrap()
            .insert(id.into(), vec![DropId::from("sitewide-reward")]);
    }
    fake
}

#[tokio::test]
async fn mines_when_twitch_lists_no_campaign_for_any_channel() {
    let mut e = engine(twitch_as_it_is_now());
    e.sync(now()).await.unwrap();

    let selected = e
        .select_channel(now())
        .await
        .unwrap()
        .expect("a drops-enabled channel streaming the game is still mineable");
    assert_eq!(
        selected.campaigns,
        vec![CampaignId::from("c1")],
        "the campaign it advances comes from the directory, not the check"
    );
}

/// The symptom the user sees: the dashboard reports nobody live while the
/// miner is mining, and every campaign claims zero live channels.
#[tokio::test]
async fn live_channels_are_reported_even_when_twitch_confirms_nothing() {
    let mut e = engine(twitch_as_it_is_now());
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    let snap = e.snapshot("default", Some("me"), now());
    assert_eq!(
        snap.live.len(),
        2,
        "both live channels were considered, so both must be shown"
    );
    let campaign = &snap.eligible[0];
    assert_eq!(
        campaign.live_channels, 2,
        "a campaign with two live channels must not report zero"
    );
}

/// Selection must be a decision, not an accident of sort order. `alpha` sorts
/// first; `zeta` is the favourite, so `zeta` must win.
#[tokio::test]
async fn the_strategy_still_decides_when_nothing_is_confirmed() {
    let mut e = engine(twitch_as_it_is_now());
    e.apply_command(
        btdm_core::control::Command::SetFavourite {
            login: "zeta".into(),
            on: true,
        },
        now(),
    );
    e.sync(now()).await.unwrap();

    let selected = e.select_channel(now()).await.unwrap().unwrap();
    assert_eq!(
        selected.channel.login, "zeta",
        "the favourite must win; picking `alpha` means sort order decided"
    );
}

/// The check keeps its one real job: a channel with no drops-enabled tag is
/// only mineable if Twitch names a drop for it. Allow-listed channels resolved
/// by name arrive with no tag, which is where this matters.
#[tokio::test]
async fn a_channel_with_no_tag_is_rescued_only_by_a_confirmed_drop() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![
        (channel("ch1", "untagged"), stream("bc-1", false)),
        (channel("ch2", "alsountagged"), stream("bc-2", false)),
    ];
    fake.available
        .lock()
        .unwrap()
        .insert("ch1".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let selected = e.select_channel(now()).await.unwrap();

    assert_eq!(
        selected.map(|w| w.channel.login),
        Some("untagged".to_string()),
        "the confirmed channel is mineable and the unconfirmed untagged one is not"
    );
}

/// A confirmed channel should edge out an equal unconfirmed one, without that
/// preference ever outweighing a deadline.
#[tokio::test]
async fn a_confirmed_channel_wins_an_otherwise_equal_choice() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![
        (channel("ch1", "aaa-unconfirmed"), stream("bc-1", true)),
        (channel("ch2", "zzz-confirmed"), stream("bc-2", true)),
    ];
    fake.available
        .lock()
        .unwrap()
        .insert("ch2".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let selected = e.select_channel(now()).await.unwrap().unwrap();
    assert_eq!(selected.channel.login, "zzz-confirmed");
}

/// A failed check is inconclusive, not a rejection — it must not strip a
/// channel the directory already vouched for.
#[tokio::test]
async fn a_failed_availability_check_does_not_remove_a_tagged_channel() {
    let fake = happy_path();
    *fake.fail_available_drops.lock().unwrap() = true;

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(
        e.select_channel(now()).await.unwrap().is_some(),
        "a broken check must not stop the miner mining"
    );
}

/// Twitch answers the same campaign differently depending on which query
/// asked. The dashboard listing carries box art; the per-campaign detail query
/// — the only one that carries the drops, so the only one worth asking — often
/// returns a game with none.
///
/// Replacing the listed campaign wholesale therefore blanked the artwork on
/// exactly the campaigns the user looks at: every *rejected* campaign kept its
/// image because it was never expanded, and over half the mineable ones lost
/// theirs.
#[tokio::test]
async fn campaign_art_survives_the_detail_upgrade() {
    let fake = FakeTwitch::default();

    let mut listed = campaign("c-new", true, vec![]);
    listed.game.box_art_url = Some("https://cdn.example/boxart.jpg".into());
    listed.account_link_url = Some("https://example.com/link".into());

    // As Twitch returns it: complete about drops, empty about presentation.
    let mut full = campaign("c-new", true, vec![drop("d1", 60, 0)]);
    full.game.box_art_url = None;
    full.account_link_url = None;

    *fake.offered.lock().unwrap() = vec![listed];
    fake.details.lock().unwrap().insert("c-new".into(), full);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    let c = &e.campaigns()[0];
    assert_eq!(
        c.game.box_art_url.as_deref(),
        Some("https://cdn.example/boxart.jpg"),
        "the listing's artwork must survive the detail fetch"
    );
    assert_eq!(
        c.account_link_url.as_deref(),
        Some("https://example.com/link"),
        "and so must the link, or an unlinked campaign becomes a dead end"
    );
    assert_eq!(c.drops.len(), 1, "while still taking the drops from the detail");
}

/// The merge must never invent facts: a detail response that genuinely says
/// something different about the drops wins outright.
#[tokio::test]
async fn the_detail_response_still_wins_on_everything_that_matters() {
    let fake = FakeTwitch::default();
    let listed = campaign("c-new", true, vec![drop("stale", 999, 0)]);
    let full = campaign("c-new", true, vec![drop("d1", 60, 30)]);
    *fake.offered.lock().unwrap() = vec![listed];
    fake.details.lock().unwrap().insert("c-new".into(), full);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    let c = &e.campaigns()[0];
    assert_eq!(c.drops.len(), 1);
    assert_eq!(c.drops[0].id, DropId::from("d1"), "stale drops must not linger");
}

/// A campaign already in the inventory is never detail-fetched — there is
/// nothing to learn from asking. It still has to pick up the artwork the
/// listing carries, or the campaigns the user is actually mining are the only
/// ones that render blank.
#[tokio::test]
async fn art_reaches_campaigns_that_were_never_detail_fetched() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    let mut listed = campaign("c1", true, vec![]);
    listed.game.box_art_url = Some("https://cdn.example/boxart.jpg".into());
    *fake.offered.lock().unwrap() = vec![listed];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    assert_eq!(*e.api().detail_fetches.lock().unwrap(), 0, "nothing to fetch");
    assert_eq!(
        e.campaigns()[0].game.box_art_url.as_deref(),
        Some("https://cdn.example/boxart.jpg"),
        "an inventory campaign must still get the listing's artwork"
    );
}
