//! HTTP-level tests.
//!
//! The unit tests cover the authorisation predicate in isolation; these prove
//! it is actually wired into every route, which is the regression that would
//! otherwise expose a live Twitch session to the network unnoticed.

use btdm_core::control::Command;
use btdm_core::snapshot::{HealthView, SettingsView, Snapshot, Totals};
use btdm_web::Hub;
use std::net::SocketAddr;

fn snapshot(account: &str) -> Snapshot {
    Snapshot {
        account: account.into(),
        login: Some("someone".into()),
        strategy: "deadline".into(),
        state: "Mining".into(),
        paused: false,
        generated_at: chrono::Utc::now(),
        watching: None,
        eligible: vec![],
        rejected: vec![],
        live: vec![],
        settings: SettingsView::default(),
        strategies: vec![],
        health: HealthView::default(),
        totals: Totals::default(),
    }
}

/// Starts the dashboard on an ephemeral port and returns its base URL.
async fn start(hub: Hub, token: Option<&str>) -> String {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let token = token.map(str::to_owned);

    tokio::spawn(async move {
        let mut tx = Some(tx);
        let _ = btdm_web::serve_announcing(hub, addr, token, move |url| {
            if let Some(tx) = tx.take() {
                let _ = tx.send(url);
            }
        })
        .await;
    });

    // The URL is announced only once the socket is bound, so awaiting it is
    // also how we know the server is ready.
    let url = rx.await.expect("server should announce its URL");
    url.split('?').next().unwrap().trim_end_matches('/').to_owned()
}

/// A stand-in miner: answers every command so the HTTP handler can complete,
/// and records what it was asked to do.
///
/// The handler waits for the miner's own reply now, so a test that took a
/// command and never answered would hang the request rather than fail.
fn fake_miner() -> (
    tokio::sync::mpsc::Sender<btdm_web::Envelope>,
    std::sync::Arc<tokio::sync::Mutex<Vec<Command>>>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<btdm_web::Envelope>(8);
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let recorder = std::sync::Arc::clone(&seen);
    tokio::spawn(async move {
        while let Some(envelope) = rx.recv().await {
            recorder.lock().await.push(envelope.command.clone());
            let _ = envelope
                .reply
                .send(btdm_core::control::CommandEffect::message("done"));
        }
    });
    (tx, seen)
}

#[tokio::test]
async fn an_unguarded_dashboard_serves_the_page_and_snapshots() {
    let hub = Hub::new();
    hub.publish(snapshot("main"));
    let base = start(hub, None).await;
    let http = reqwest::Client::new();

    let page = http.get(&base).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("<!doctype html>"));

    let snaps: Vec<Snapshot> = http
        .get(format!("{base}/api/snapshots"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].account, "main");
}

/// A probe that always says "ok" is useless. Before any miner has reported,
/// the honest answer is "starting", not "healthy".
#[tokio::test]
async fn health_is_not_ok_before_any_miner_reports() {
    let base = start(Hub::new(), None).await;
    let res = reqwest::get(format!("{base}/api/health")).await.unwrap();
    assert_eq!(res.status(), 503);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["status"], "starting");
}

#[tokio::test]
async fn health_reports_mining() {
    let hub = Hub::new();
    hub.publish(snapshot("main"));
    let base = start(hub, None).await;

    let res = reqwest::get(format!("{base}/api/health")).await.unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["status"], "mining");
    assert_eq!(body["accounts"], 1);
}

/// The point of a probe is that it fires when mining has stopped earning.
/// This is what a systemd or Docker healthcheck hangs off.
#[tokio::test]
async fn health_fails_when_a_miner_is_stalled() {
    let hub = Hub::new();
    let mut s = snapshot("main");
    s.watching = Some(btdm_core::snapshot::WatchingView {
        channel_login: "someone".into(),
        channel_display_name: "Someone".into(),
        stream_title: String::new(),
        viewers: 0,
        campaign: "Camp".into(),
        campaigns: vec!["Camp".into()],
        reason: String::new(),
        since: chrono::Utc::now(),
        drop_name: None,
        current_minutes: None,
        required_minutes: None,
        seconds_since_confirmation: Some(900),
        stalled: true,
    });
    hub.publish(s);
    let base = start(hub, None).await;

    let res = reqwest::get(format!("{base}/api/health")).await.unwrap();
    assert_eq!(res.status(), 503, "a stalled miner must fail the probe");
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["status"], "stalled");
    assert_eq!(body["stalled"], 1);
}

/// Pausing is a decision the user made, not a fault.
#[tokio::test]
async fn a_paused_miner_is_healthy() {
    let hub = Hub::new();
    let mut s = snapshot("main");
    s.paused = true;
    s.state = "Paused".into();
    hub.publish(s);
    let base = start(hub, None).await;

    let res = reqwest::get(format!("{base}/api/health")).await.unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["status"], "paused");
}

/// It is the only unauthenticated route, so it must leak nothing.
#[tokio::test]
async fn health_reveals_no_account_details() {
    let hub = Hub::new();
    hub.publish(snapshot("my-secret-account"));
    let base = start(hub, Some("secret")).await;

    let text = reqwest::get(format!("{base}/api/health"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!text.contains("my-secret-account"), "leaked: {text}");
    assert!(!text.contains("someone"), "leaked the Twitch login: {text}");
}

/// The guard must be on every route, not just the page. A snapshot leaks which
/// account is linked and what it is watching.
#[tokio::test]
async fn every_route_is_guarded_when_a_token_is_set() {
    let hub = Hub::new();
    hub.publish(snapshot("main"));
    let base = start(hub, Some("secret")).await;
    let http = reqwest::Client::new();

    for path in ["", "/api/snapshots", "/api/events"] {
        let res = http.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(
            res.status(),
            401,
            "{path} must not be reachable without the token"
        );
    }
}

#[tokio::test]
async fn the_correct_token_grants_access_by_query_and_by_header() {
    let hub = Hub::new();
    hub.publish(snapshot("main"));
    let base = start(hub, Some("secret")).await;
    let http = reqwest::Client::new();

    let via_query = http
        .get(format!("{base}/api/snapshots?token=secret"))
        .send()
        .await
        .unwrap();
    assert_eq!(via_query.status(), 200);

    let via_header = http
        .get(format!("{base}/api/snapshots"))
        .header("Authorization", "Bearer secret")
        .send()
        .await
        .unwrap();
    assert_eq!(via_header.status(), 200);
}

#[tokio::test]
async fn a_wrong_token_is_rejected() {
    let base = start(Hub::new(), Some("secret")).await;
    let res = reqwest::get(format!("{base}/api/snapshots?token=nope"))
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn snapshots_are_empty_before_any_miner_reports() {
    let base = start(Hub::new(), None).await;
    let snaps: Vec<Snapshot> = reqwest::get(format!("{base}/api/snapshots"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(snaps.is_empty());
}

// ---------------------------------------------------------------------------
// The control plane
//
// A dashboard whose buttons quietly do nothing is worse than one with no
// buttons, so each of these proves a command actually reaches the miner.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_command_reaches_the_registered_miner() {
    let hub = Hub::new();
    let (tx, seen) = fake_miner();
    hub.register("main", tx);
    let base = start(hub, None).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/accounts/main/command"))
        .json(&Command::SetStrategy {
            name: "coverage".into(),
        })
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    assert_eq!(
        seen.lock().await.as_slice(),
        [Command::SetStrategy { name: "coverage".into() }]
    );
}

#[tokio::test]
async fn commanding_an_account_that_is_not_running_is_a_404() {
    let base = start(Hub::new(), None).await;
    let res = reqwest::Client::new()
        .post(format!("{base}/api/accounts/ghost/command"))
        .json(&Command::Pause)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn a_malformed_command_is_rejected_rather_than_ignored() {
    let hub = Hub::new();
    let (tx, _seen) = fake_miner();
    hub.register("main", tx);
    let base = start(hub, None).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/accounts/main/command"))
        .header("content-type", "application/json")
        .body(r#"{"command":"self_destruct"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
}

/// Commands change what the miner does with a live Twitch session, so they
/// must be at least as guarded as reading state.
#[tokio::test]
async fn commands_require_the_token() {
    let hub = Hub::new();
    let (tx, seen) = fake_miner();
    hub.register("main", tx);
    let base = start(hub, Some("secret")).await;
    let http = reqwest::Client::new();

    let rejected = http
        .post(format!("{base}/api/accounts/main/command"))
        .json(&Command::Pause)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 401);

    let accepted = http
        .post(format!("{base}/api/accounts/main/command?token=secret"))
        .json(&Command::Pause)
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), 200);
    assert_eq!(seen.lock().await.as_slice(), [Command::Pause]);
}

/// The page renders from pushed snapshots. If the stream does not open with
/// the current state, a freshly loaded dashboard sits blank until something
/// changes — which, on a healthy miner, can be a full minute.
#[tokio::test]
async fn the_event_stream_opens_with_the_current_state() {
    let hub = Hub::new();
    hub.publish(snapshot("main"));
    let base = start(hub, None).await;

    let mut res = reqwest::get(format!("{base}/api/events")).await.unwrap();

    let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), res.chunk())
        .await
        .expect("the stream must send the current state immediately")
        .unwrap()
        .expect("a first chunk");

    let text = String::from_utf8_lossy(&chunk);
    assert!(text.contains("event: snapshot"), "got: {text}");
    assert!(text.contains("\"account\":\"main\""), "got: {text}");
}

/// A live snapshot must arrive without the page asking for it. Polling was the
/// old behaviour and is why changes took up to fifteen seconds to appear.
#[tokio::test]
async fn a_published_snapshot_is_pushed_to_an_open_stream() {
    let hub = Hub::new();
    let base = start(hub.clone(), None).await;

    let mut res = reqwest::get(format!("{base}/api/events")).await.unwrap();

    // Nothing has been published yet, so the stream is open but idle.
    let mut updated = snapshot("main");
    updated.state = "Idle".into();
    hub.publish(updated);

    let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), res.chunk())
        .await
        .expect("a published snapshot must reach an open stream")
        .unwrap()
        .expect("a chunk");

    let text = String::from_utf8_lossy(&chunk);
    assert!(text.contains("event: snapshot"), "got: {text}");
    assert!(text.contains("\"state\":\"Idle\""), "got: {text}");
}

// ---------------------------------------------------------------------------
// Stored history
//
// The miner writes every event to SQLite. Until these routes existed the page
// could not read any of it, so opening the dashboard after a restart showed an
// empty log and no record of anything ever claimed.
// ---------------------------------------------------------------------------

struct FakeHistory;

impl btdm_web::History for FakeHistory {
    fn recent(&self, account: &str, limit: u32) -> Vec<btdm_web::HistoryRow> {
        if account != "main" {
            return vec![];
        }
        (0..limit.min(3))
            .map(|i| btdm_web::HistoryRow {
                at: "2026-08-10T12:00:00Z".into(),
                summary: format!("line {i}"),
                severity: "info".into(),
            })
            .collect()
    }

    fn rewards(&self, account: &str, _limit: u32) -> Vec<btdm_web::RewardRow> {
        if account != "main" {
            return vec![];
        }
        vec![btdm_web::RewardRow {
            at: "2026-08-10T12:00:00Z".into(),
            name: "Weapon Charm".into(),
        }]
    }
}

#[tokio::test]
async fn history_is_served_per_account() {
    let hub = Hub::new();
    hub.set_history(std::sync::Arc::new(FakeHistory));
    let base = start(hub, None).await;

    let rows: Vec<serde_json::Value> =
        reqwest::get(format!("{base}/api/history?account=main&limit=2"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(rows.len(), 2, "the limit must be honoured");
    assert_eq!(rows[0]["summary"], "line 0");

    let other: Vec<serde_json::Value> =
        reqwest::get(format!("{base}/api/history?account=someone-else"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert!(other.is_empty(), "accounts must not see each other's log");
}

#[tokio::test]
async fn rewards_are_served() {
    let hub = Hub::new();
    hub.set_history(std::sync::Arc::new(FakeHistory));
    let base = start(hub, None).await;

    let rows: Vec<serde_json::Value> = reqwest::get(format!("{base}/api/rewards?account=main"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Weapon Charm");
}

/// With no history attached the routes must return an empty list rather than
/// erroring — `btdm mine` can run without a database behind the dashboard.
#[tokio::test]
async fn history_without_a_store_is_empty_not_broken() {
    let base = start(Hub::new(), None).await;
    let res = reqwest::get(format!("{base}/api/history?account=main")).await.unwrap();
    assert_eq!(res.status(), 200);
    let rows: Vec<serde_json::Value> = res.json().await.unwrap();
    assert!(rows.is_empty());
}

/// History is derived from a live Twitch session and must be guarded like
/// everything else.
#[tokio::test]
async fn history_requires_the_token() {
    let hub = Hub::new();
    hub.set_history(std::sync::Arc::new(FakeHistory));
    let base = start(hub, Some("secret")).await;

    for path in ["/api/history?account=main", "/api/rewards?account=main"] {
        let res = reqwest::get(format!("{base}{path}")).await.unwrap();
        assert_eq!(res.status(), 401, "{path} must be guarded");
    }
    let ok = reqwest::get(format!("{base}/api/history?account=main&token=secret"))
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
}

// ---------------------------------------------------------------------------
// Logging in from the dashboard
//
// A dead token used to need shell access, which is no answer on the headless
// box this targets — and by then the miner had already exited, so there was
// nothing left to ask.
// ---------------------------------------------------------------------------

struct FakeEnrolment {
    started: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    refuse: bool,
}

impl btdm_web::Enrolment for FakeEnrolment {
    fn begin(&self, account: &str) -> Result<(), String> {
        if self.refuse {
            return Err("already in progress".into());
        }
        self.started.lock().unwrap().push(account.to_owned());
        Ok(())
    }
}

#[tokio::test]
async fn a_login_can_be_started_from_the_dashboard() {
    let hub = Hub::new();
    let started = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    hub.set_enrolment(std::sync::Arc::new(FakeEnrolment {
        started: started.clone(),
        refuse: false,
    }));
    let base = start(hub, None).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/accounts/main/login"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    assert_eq!(*started.lock().unwrap(), vec!["main".to_string()]);
}

#[tokio::test]
async fn a_refused_login_says_why() {
    let hub = Hub::new();
    hub.set_enrolment(std::sync::Arc::new(FakeEnrolment {
        started: Default::default(),
        refuse: true,
    }));
    let base = start(hub, None).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/api/accounts/main/login"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"], "already in progress");
}

/// The code has to reach the page, or the flow is unusable.
#[tokio::test]
async fn a_pending_code_is_visible_to_the_page() {
    let hub = Hub::new();
    hub.set_login_state(btdm_web::LoginState {
        account: "main".into(),
        status: "pending".into(),
        user_code: Some("ABCD1234".into()),
        verification_uri: Some("https://www.twitch.tv/activate".into()),
        detail: None,
    });
    hub.set_account_error("other", "the stored token was rejected");
    let base = start(hub, None).await;

    let body: serde_json::Value = reqwest::get(format!("{base}/api/logins"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["logins"][0]["user_code"], "ABCD1234");
    assert_eq!(body["errors"]["other"], "the stored token was rejected");
}

/// Starting a login changes what the miner does with a Twitch session, so it
/// is guarded like every other mutating route.
#[tokio::test]
async fn login_routes_require_the_token() {
    let hub = Hub::new();
    hub.set_enrolment(std::sync::Arc::new(FakeEnrolment {
        started: Default::default(),
        refuse: false,
    }));
    let base = start(hub, Some("secret")).await;
    let http = reqwest::Client::new();

    assert_eq!(
        http.post(format!("{base}/api/accounts/main/login"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        reqwest::get(format!("{base}/api/logins")).await.unwrap().status(),
        401
    );
}

/// With no enrolment attached the route must refuse clearly rather than 404 or
/// pretend to have worked.
#[tokio::test]
async fn a_miner_that_cannot_enrol_says_so() {
    let base = start(Hub::new(), None).await;
    let res = reqwest::Client::new()
        .post(format!("{base}/api/accounts/main/login"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
}

// ---------------------------------------------------------------------------
// Hostile HTTP
//
// Everything here is what an unfriendly client, a confused proxy or a broken
// script would send. None of it may take the dashboard down, and none of it
// may reach past the authorisation guard.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_giant_body_is_rejected_without_taking_the_server_down() {
    let hub = Hub::new();
    let (tx, _seen) = fake_miner();
    hub.register("main", tx);
    let base = start(hub, None).await;
    let http = reqwest::Client::new();

    let huge = "x".repeat(8 * 1024 * 1024);
    let res = http
        .post(format!("{base}/api/accounts/main/command"))
        .header("content-type", "application/json")
        .body(format!(r#"{{"command":"set_strategy","name":"{huge}"}}"#))
        .send()
        .await;

    // Rejected or accepted, but the server must still be alive afterwards.
    let _ = res;
    let health = reqwest::get(format!("{base}/api/health")).await.unwrap();
    assert!(health.status().is_success() || health.status().as_u16() == 503);
}

#[tokio::test]
async fn deeply_nested_json_does_not_blow_the_stack() {
    let hub = Hub::new();
    let (tx, _seen) = fake_miner();
    hub.register("main", tx);
    let base = start(hub, None).await;

    let nested = format!("{}{}", "[".repeat(20_000), "]".repeat(20_000));
    let res = reqwest::Client::new()
        .post(format!("{base}/api/accounts/main/command"))
        .header("content-type", "application/json")
        .body(nested)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);

    // Still serving.
    assert!(
        reqwest::get(format!("{base}/api/snapshots"))
            .await
            .unwrap()
            .status()
            .is_success()
    );
}

/// An account name is a path segment. It must not be able to address another
/// route, and it must not be able to reach an account that is not registered.
#[tokio::test]
async fn an_account_name_cannot_traverse_the_path() {
    let hub = Hub::new();
    let (tx, seen) = fake_miner();
    hub.register("main", tx);
    let base = start(hub, None).await;
    let http = reqwest::Client::new();

    for hostile in [
        "..%2F..%2Fmain",
        "%2E%2E%2Fmain",
        "main%00",
        "MAIN",
        "main%20",
    ] {
        let res = http
            .post(format!("{base}/api/accounts/{hostile}/command"))
            .json(&Command::Pause)
            .send()
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            404,
            "'{hostile}' must not resolve to the registered account"
        );
    }
    assert!(seen.lock().await.is_empty(), "nothing should have reached the miner");
}

/// Names Twitch actually permits must still work.
#[tokio::test]
async fn an_unusual_but_legitimate_account_name_works() {
    let hub = Hub::new();
    let (tx, seen) = fake_miner();
    hub.register("my account 日本語", tx);
    let base = start(hub, None).await;

    let res = reqwest::Client::new()
        .post(format!(
            "{base}/api/accounts/{}/command",
            urlencoding_lite("my account 日本語")
        ))
        .json(&Command::Pause)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(seen.lock().await.as_slice(), [Command::Pause]);
}

/// Minimal percent-encoding, so the test does not need a dependency.
fn urlencoding_lite(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Wrong types in the right shape must be refused, not coerced into something
/// the miner then acts on.
#[tokio::test]
async fn commands_with_wrong_types_are_refused() {
    let hub = Hub::new();
    let (tx, seen) = fake_miner();
    hub.register("main", tx);
    let base = start(hub, None).await;
    let http = reqwest::Client::new();

    for body in [
        r#"{"command":"set_strategy","name":123}"#,
        r#"{"command":"set_strategy"}"#,
        r#"{"command":"set_favourite","login":"x"}"#,
        r#"{"command":"set_priority_games","games":"not an array"}"#,
        r#"{"command":"pin","channel":[]}"#,
        r#"{"command":null}"#,
        r#"[]"#,
        r#""pause""#,
        r#"null"#,
        "",
    ] {
        let res = http
            .post(format!("{base}/api/accounts/main/command"))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400, "body {body:?} should be refused");
    }
    assert!(seen.lock().await.is_empty(), "nothing malformed may reach the miner");
}

/// A queue that has filled up must say so rather than silently dropping the
/// user's command.
#[tokio::test]
async fn a_full_command_queue_is_reported() {
    let hub = Hub::new();
    // A miner that never reads its queue: the one command it can hold sits
    // there, and the next has nowhere to go.
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    hub.register("main", tx);
    let base = start(hub, None).await;
    let http = reqwest::Client::new();

    // The first fills the queue of one. Its own request stays open waiting for
    // an answer that is never coming, so it is left in flight rather than
    // awaited — the assertion here is about the *second*.
    let filling = tokio::spawn({
        let base = base.clone();
        async move {
            let _ = reqwest::Client::new()
                .post(format!("{base}/api/accounts/main/command"))
                .json(&Command::Pause)
                .send()
                .await;
        }
    });
    // Give it time to occupy the slot.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let second = http
        .post(format!("{base}/api/accounts/main/command"))
        .json(&Command::Pause)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 503, "a dropped command must not look accepted");
    filling.abort();
}

/// A miner that takes a command and then wedges must not hold the request open
/// forever: the page would simply stop responding to clicks. It is reported as
/// accepted-but-unconfirmed, which is the truth.
#[tokio::test(start_paused = true)]
async fn a_miner_that_never_answers_does_not_hang_the_page() {
    let hub = Hub::new();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    hub.register("main", tx);

    let result = hub.send_command("main", Command::Pause).await;
    match result {
        Err(btdm_web::CommandError::Slow(who)) => assert_eq!(who, "main"),
        other => panic!("expected a bounded wait, got {other:?}"),
    }
}

/// Many dashboards open at once must not starve each other or the miner.
#[tokio::test]
async fn many_concurrent_readers_are_served() {
    let hub = Hub::new();
    hub.publish(snapshot("main"));
    let base = start(hub, None).await;

    let mut handles = Vec::new();
    for _ in 0..50 {
        let url = format!("{base}/api/snapshots");
        handles.push(tokio::spawn(async move {
            reqwest::get(url).await.unwrap().status().as_u16()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), 200);
    }
}

/// A token must not be accepted merely because it is a prefix, a suffix, or
/// present under a different key.
#[tokio::test]
async fn token_comparison_cannot_be_fooled() {
    let base = start(Hub::new(), Some("secret")).await;
    let http = reqwest::Client::new();

    for query in [
        "token=sec",
        "token=secretx",
        "token=xsecret",
        "tokenx=secret",
        "atoken=secret",
        "token=SECRET",
        "token=",
        "token=secret%20",
    ] {
        let res = http
            .get(format!("{base}/api/snapshots?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "'{query}' must not authorise");
    }

    // ...and the real one still does, wherever it sits in the query string.
    for query in ["token=secret", "a=1&token=secret", "token=secret&b=2"] {
        let res = http
            .get(format!("{base}/api/snapshots?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "'{query}' should authorise");
    }
}

/// An unknown route must not fall through to something that serves state.
#[tokio::test]
async fn unknown_routes_are_not_served() {
    let hub = Hub::new();
    hub.publish(snapshot("main"));
    let base = start(hub, Some("secret")).await;

    for path in [
        "/api",
        "/api/",
        "/api/snapshot",
        "/api/snapshots/",
        "/../index.html",
        "/api/accounts//command",
        "/api/accounts/main",
    ] {
        let res = reqwest::get(format!("{base}{path}?token=secret"))
            .await
            .unwrap();
        assert!(
            res.status().as_u16() == 404 || res.status().as_u16() == 405,
            "{path} returned {}",
            res.status()
        );
    }
}

/// Reading state must never be possible through a method that skips the guard.
#[tokio::test]
async fn other_methods_do_not_bypass_the_guard() {
    let hub = Hub::new();
    hub.publish(snapshot("main"));
    let base = start(hub, Some("secret")).await;
    let http = reqwest::Client::new();

    for method in [reqwest::Method::POST, reqwest::Method::PUT, reqwest::Method::DELETE] {
        let res = http
            .request(method.clone(), format!("{base}/api/snapshots"))
            .send()
            .await
            .unwrap();
        assert!(
            res.status().as_u16() != 200,
            "{method} on /api/snapshots returned 200"
        );
    }

    // HEAD is a read, and must be guarded exactly like GET.
    let head = http
        .request(reqwest::Method::HEAD, format!("{base}/api/snapshots"))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 401);
}
