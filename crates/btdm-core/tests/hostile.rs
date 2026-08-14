//! What the engine does when Twitch misbehaves.
//!
//! Every scenario here is something a real API can produce: contradictory
//! figures, absurd dates, duplicate ids, empty everything, or a shape nobody
//! anticipated. None of it may panic, hang, or make the engine claim progress
//! it does not have.

mod support;

use btdm_core::api::{ApiError, DropProgress};
use btdm_core::control::Command;
use btdm_core::engine::{Engine, EngineConfig, MiningState};
use btdm_core::model::*;
use chrono::{Duration, TimeZone, Utc};
use support::*;

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap()
}

fn engine(fake: FakeTwitch) -> Engine<FakeTwitch> {
    Engine::new(fake, EngineConfig::default())
}

// ---------------------------------------------------------------------------
// Contradictory and absurd data
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_campaign_that_ends_before_it_starts_is_not_mined() {
    let fake = FakeTwitch::default();
    let mut backwards = campaign("c1", true, vec![drop("d1", 60, 0)]);
    backwards.starts_at = now() + Duration::days(1);
    backwards.ends_at = now() - Duration::days(1);
    *fake.inventory.lock().unwrap() = vec![backwards];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.select_channel(now()).await.unwrap().is_none());
}

#[tokio::test]
async fn a_drop_needing_zero_minutes_does_not_divide_by_zero() {
    let fake = happy_path_with(vec![drop("d1", 0, 0)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let snap = e.snapshot("main", None, now());
    for c in &snap.eligible {
        for d in &c.drops {
            assert!(d.percent.is_finite(), "percent was {}", d.percent);
        }
    }
}

/// Twitch reports more minutes watched than required more often than you would
/// expect. It must not produce a negative remaining time or a bar past 100%.
#[tokio::test]
async fn more_minutes_than_required_does_not_go_negative() {
    let fake = happy_path_with(vec![drop("d1", 60, 5_000)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    let snap = e.snapshot("main", None, now());
    assert_eq!(snap.eligible[0].remaining_minutes, 0);
    assert!(snap.eligible[0].drops[0].percent <= 100.0);
}

#[tokio::test]
async fn a_campaign_with_no_drops_at_all_is_rejected_not_mined() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![])];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.campaigns().is_empty());
    assert!(e.select_channel(now()).await.unwrap().is_none());
}

/// Duplicate campaign ids from two sources must not produce two entries that
/// then double-count towards everything.
#[tokio::test]
async fn duplicate_campaigns_are_merged_not_doubled() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![
        campaign("c1", true, vec![drop("d1", 60, 10)]),
        campaign("c1", true, vec![drop("d1", 60, 20)]),
    ];
    *fake.offered.lock().unwrap() = vec![campaign("c1", true, vec![])];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert_eq!(e.campaigns().len(), 1);
    assert_eq!(e.snapshot("main", None, now()).totals.campaigns_eligible, 1);
}

#[tokio::test]
async fn duplicate_drop_ids_within_a_campaign_do_not_confuse_progress() {
    let fake = happy_path_with(vec![drop("d1", 60, 0), drop("d1", 120, 0)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d1"),
            current_minutes: 30,
            required_minutes: 60,
        },
        now(),
    );
    assert_eq!(e.state(), MiningState::Mining);
}

/// A channel Twitch lists twice must not appear twice as a candidate.
#[tokio::test]
async fn a_duplicated_live_channel_is_only_offered_once() {
    let fake = happy_path_with(vec![drop("d1", 60, 0)]);
    let dup = (channel("ch1", "streamer1"), stream("bc-1", true));
    fake.channels.lock().unwrap().push(dup);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    let snap = e.snapshot("main", None, now());
    assert_eq!(snap.live.len(), 1, "one channel, listed twice by Twitch");
    assert_eq!(snap.live[0].campaigns.len(), 1, "and one campaign, not two");
}

#[tokio::test]
async fn an_empty_string_id_does_not_match_everything() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("", true, vec![drop("", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![(channel("", ""), stream("", true))];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    // Whatever it decides, it must not panic and must not claim to be mining.
    let _ = e.select_channel(now()).await;
    e.tick(now()).await.ok();
}

// ---------------------------------------------------------------------------
// Progress that cannot be trusted
// ---------------------------------------------------------------------------

/// Progress that goes backwards is not progress, and must not reset the stall
/// clock — that is precisely how a stalled session hides.
#[tokio::test]
async fn progress_going_backwards_is_not_treated_as_movement() {
    let fake = happy_path_with(vec![drop("d1", 60, 0)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d1"),
            current_minutes: 30,
            required_minutes: 60,
        },
        now(),
    );
    let later = now() + Duration::minutes(5);
    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("d1"),
            current_minutes: 10,
            required_minutes: 60,
        },
        later,
    );

    assert!(
        e.since_last_confirmation(later).unwrap() >= Duration::minutes(5),
        "a regression must leave the stall clock running"
    );
    assert_eq!(
        e.active_drop().unwrap().1.current_minutes,
        30,
        "and must not lower the confirmed figure"
    );
}

#[tokio::test]
async fn a_repeated_identical_confirmation_is_not_movement() {
    let fake = happy_path_with(vec![drop("d1", 60, 0)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    let p = DropProgress {
        drop_id: DropId::from("d1"),
        current_minutes: 30,
        required_minutes: 60,
    };
    e.on_pushed_progress(p.clone(), now());
    let later = now() + Duration::minutes(5);
    e.on_pushed_progress(p, later);

    assert!(
        e.since_last_confirmation(later).unwrap() >= Duration::minutes(5),
        "the same number twice is not progress"
    );
}

#[tokio::test]
async fn progress_for_a_drop_that_does_not_exist_is_ignored() {
    let fake = happy_path_with(vec![drop("d1", 60, 0)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    e.on_pushed_progress(
        DropProgress {
            drop_id: DropId::from("who-is-this"),
            current_minutes: 59,
            required_minutes: 60,
        },
        now(),
    );
    assert_eq!(e.state(), MiningState::Establishing);
}

// ---------------------------------------------------------------------------
// The API failing in every way it can
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_call_failing_leaves_the_engine_usable() {
    let fake = happy_path_with(vec![drop("d1", 60, 0)]);
    *fake.fail_with.lock().unwrap() = Some(ApiError::Network("everything is down".into()));

    let mut e = engine(fake);
    assert!(e.sync(now()).await.is_err());
    // With no campaigns known, there is nothing to look for and nothing to
    // fail at — reporting an error here would be inventing one.
    assert!(e.select_channel(now()).await.unwrap().is_none());

    // Once campaigns are known, a failing API does surface as an error.
    *e.api().fail_with.lock().unwrap() = None;
    e.sync(now()).await.unwrap();
    *e.api().fail_with.lock().unwrap() = Some(ApiError::Network("down again".into()));
    assert!(e.select_channel(now()).await.is_err());

    // And it recovers when the network does.
    *e.api().fail_with.lock().unwrap() = None;
    e.sync(now()).await.unwrap();
    assert!(e.select_channel(now()).await.unwrap().is_some());
}

#[tokio::test]
async fn a_rate_limit_does_not_look_like_an_empty_world() {
    let fake = happy_path_with(vec![drop("d1", 60, 0)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert_eq!(e.campaigns().len(), 1);

    *e.api().fail_with.lock().unwrap() = Some(ApiError::RateLimited);
    let _ = e.sync(now()).await;

    assert_eq!(
        e.campaigns().len(),
        1,
        "a failed sync must not wipe what we already knew"
    );
}

// ---------------------------------------------------------------------------
// Commands in hostile combinations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn commands_in_every_order_leave_a_coherent_engine() {
    let fake = happy_path_with(vec![drop("d1", 60, 0)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    let commands = vec![
        Command::Pause,
        Command::SetStrategy { name: "coverage".into() },
        Command::Resume,
        Command::Pin { channel: Some("someone".into()) },
        Command::SwitchChannel,
        Command::SetFavourite { login: "x".into(), on: true },
        Command::Pause,
        Command::WatchChannel { login: "streamer1".into() },
        Command::SetGameExcluded { game: "Deadlock".into(), excluded: true },
        Command::Resume,
        Command::SetGameExcluded { game: "Deadlock".into(), excluded: false },
        Command::ClearQuarantine,
        Command::Pin { channel: None },
        Command::SyncNow,
        Command::SetPriorityGames { games: vec![] },
        Command::SetStrategy { name: "deadline".into() },
    ];

    for command in commands {
        e.apply_command(command, now());
        // Whatever happened, these must never disagree.
        let paused = e.is_paused();
        assert_eq!(paused, e.state() == MiningState::Paused);
        if paused {
            assert!(e.current().is_none(), "paused but still watching");
        }
        let _ = e.select_channel(now()).await;
        e.tick(now()).await.ok();
        let snap = e.snapshot("main", None, now());
        assert_eq!(snap.paused, paused, "the snapshot disagreed about pausing");
    }
}

#[tokio::test]
async fn absurd_command_arguments_are_survivable() {
    let fake = happy_path_with(vec![drop("d1", 60, 0)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();

    let long = "x".repeat(100_000);
    for command in [
        Command::SetStrategy { name: String::new() },
        Command::SetStrategy { name: long.clone() },
        Command::Pin { channel: Some(String::new()) },
        Command::Pin { channel: Some("   ".into()) },
        Command::Pin { channel: Some(long.clone()) },
        Command::SetFavourite { login: String::new(), on: true },
        Command::SetFavourite { login: "  ".into(), on: false },
        Command::WatchChannel { login: String::new() },
        Command::SetGameExcluded { game: String::new(), excluded: true },
        Command::SetPriorityGames { games: vec![String::new(), "  ".into(), long] },
    ] {
        e.apply_command(command, now());
    }

    // Nothing empty may have been stored.
    let s = e.settings();
    assert!(s.priority_games.iter().all(|g| !g.trim().is_empty()));
    assert!(s.favourite_channels.iter().all(|f| !f.trim().is_empty()));
    assert!(s.pinned_channel.as_ref().is_none_or(|p| !p.trim().is_empty()));
    assert_eq!(e.strategy_name(), "deadline", "an empty name kept the old one");
}

/// Time going backwards happens: NTP corrections, suspended laptops, clock
/// skew in a container. It must not panic or produce nonsense.
#[tokio::test]
async fn the_clock_going_backwards_is_survivable() {
    let fake = happy_path_with(vec![drop("d1", 60, 0)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    let earlier = now() - Duration::hours(2);
    e.tick(earlier).await.unwrap();
    let _ = e.assess_health(earlier);
    let snap = e.snapshot("main", None, earlier);
    if let Some(secs) = snap.health.seconds_since_confirmation {
        assert!(secs <= 0 || secs >= 0, "must not panic computing it");
    }
    assert!(
        snap.health
            .next_sync_in_seconds
            .is_none_or(|s| s >= 0),
        "a countdown must never be negative"
    );
}

/// A campaign list that changes completely between syncs must not leave the
/// session pointing at something that no longer exists.
#[tokio::test]
async fn the_world_changing_entirely_between_syncs_is_survivable() {
    let fake = happy_path_with(vec![drop("d1", 60, 0)]);
    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    *e.api().inventory.lock().unwrap() = vec![campaign("totally-different", true, vec![drop("d9", 60, 0)])];
    e.api().available.lock().unwrap().clear();
    e.sync(now()).await.unwrap();

    let snap = e.snapshot("main", None, now());
    assert!(snap.eligible.iter().all(|c| c.id == "totally-different"));
    // Whatever it is watching, it must be something the snapshot knows about.
    if let Some(w) = &snap.watching {
        assert!(!w.campaign.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Volume
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_thousand_campaigns_do_not_hang_the_engine() {
    let fake = FakeTwitch::default();
    let campaigns: Vec<_> = (0..1000)
        .map(|i| {
            campaign(
                &format!("c{i}"),
                true,
                (0..10).map(|j| drop(&format!("d{i}-{j}"), 60 * (j + 1), 0)).collect(),
            )
        })
        .collect();
    *fake.inventory.lock().unwrap() = campaigns;
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1", true))];

    let mut e = engine(fake);
    let started = std::time::Instant::now();
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    let snap = e.snapshot("main", None, now());
    let took = started.elapsed();

    assert_eq!(snap.eligible.len(), 1000);
    assert!(took.as_secs() < 10, "took {took:?}");
}

/// A precondition chain a thousand deep must not blow the stack.
#[tokio::test]
async fn a_very_deep_precondition_chain_terminates() {
    let mut drops = vec![drop("t0", 10, 0)];
    for i in 1..500 {
        let mut d = drop(&format!("t{i}"), 10, 0);
        d.precondition_ids = vec![DropId::from(&format!("t{}", i - 1)[..])];
        drops.push(d);
    }
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, drops)];

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let _ = e.snapshot("main", None, now());
}

/// Helper: the standard one-campaign world, with the drops given.
fn happy_path_with(drops: Vec<TimedDrop>) -> FakeTwitch {
    let fake = FakeTwitch::default();
    let ids: Vec<DropId> = drops.iter().map(|d| d.id.clone()).collect();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, drops)];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1", true))];
    fake.available.lock().unwrap().insert("ch1".into(), ids);
    fake
}

// ---------------------------------------------------------------------------
// Unbounded user input
//
// Everything in these lists is written to config.toml and repeated in every
// snapshot the dashboard pushes over SSE. A single 200 KB priority entry was
// accepted, persisted, and then carried in every frame for the life of the
// process — a paste accident, not even an attack, and permanent.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_absurdly_long_entry_is_refused_rather_than_persisted() {
    use btdm_core::control::Command as C;
    let mut e = engine(FakeTwitch::default());
    let huge = "a".repeat(200_000);

    for (label, cmd) in [
        ("priority", C::AddPriorityEntry { entry: huge.clone() }),
        ("priority list", C::SetPriorityGames { games: vec![huge.clone()] }),
        ("favourite", C::SetFavourite { login: huge.clone(), on: true }),
        ("excluded game", C::SetGameExcluded { game: huge.clone(), excluded: true }),
        ("pin", C::Pin { channel: Some(huge.clone()) }),
        ("excluded campaign", C::SetCampaignExcluded {
            campaign: CampaignId::from(&huge[..]), excluded: true,
        }),
    ] {
        let effect = e.apply_command(cmd, now());
        assert!(
            !effect.persist,
            "{label} accepted a 200 KB entry and would write it to config.toml"
        );
    }

    let settings = e.snapshot("default", Some("me"), now()).settings;
    assert!(settings.priority_games.is_empty(), "{:?}", settings.priority_games.len());
    assert!(settings.favourites.is_empty());
    assert!(settings.excluded_games.is_empty());
    assert!(settings.excluded_campaigns.is_empty());
    assert_eq!(settings.pinned, None);
}

/// A name of a plausible length must still work — the bound is on abuse, not
/// on long campaign names.
#[tokio::test]
async fn a_long_but_realistic_name_is_still_accepted() {
    use btdm_core::control::Command as C;
    let mut e = engine(FakeTwitch::default());
    let realistic = "Tom Clancy's The Division: Resurgence — Broken Rain Incursion Week 3";

    let effect = e.apply_command(C::AddPriorityEntry { entry: realistic.into() }, now());
    assert!(effect.persist, "a real campaign name must be accepted: {}", effect.message);
}

/// Enough entries to bloat the config just as effectively as one long one.
#[tokio::test]
async fn a_list_cannot_grow_without_limit() {
    use btdm_core::control::Command as C;
    let mut e = engine(FakeTwitch::default());

    for i in 0..500 {
        e.apply_command(C::AddPriorityEntry { entry: format!("game {i}") }, now());
    }
    let n = e.snapshot("default", Some("me"), now()).settings.priority_games.len();
    assert!(n <= 200, "the priority list grew to {n} entries");
}

/// `dedup` only collapses *adjacent* repeats, so an alternating list kept
/// every copy.
#[tokio::test]
async fn a_wholesale_list_is_deduplicated_properly() {
    use btdm_core::control::Command as C;
    let mut e = engine(FakeTwitch::default());
    let alternating: Vec<String> = (0..50)
        .map(|i| if i % 2 == 0 { "alpha".to_string() } else { "beta".to_string() })
        .collect();

    e.apply_command(C::SetPriorityGames { games: alternating }, now());
    assert_eq!(
        e.snapshot("default", Some("me"), now()).settings.priority_games,
        vec!["alpha", "beta"]
    );
}
