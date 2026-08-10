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
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
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
        rx.recv().await.unwrap(),
        Command::SetStrategy {
            name: "coverage".into()
        }
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
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
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
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
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
    assert_eq!(rx.recv().await.unwrap(), Command::Pause);
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
