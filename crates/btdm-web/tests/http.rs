//! HTTP-level tests.
//!
//! The unit tests cover the authorisation predicate in isolation; these prove
//! it is actually wired into every route, which is the regression that would
//! otherwise expose a live Twitch session to the network unnoticed.

use btdm_core::snapshot::{Snapshot, Totals};
use btdm_web::Hub;
use std::net::SocketAddr;

fn snapshot(account: &str) -> Snapshot {
    Snapshot {
        account: account.into(),
        login: Some("someone".into()),
        strategy: "deadline".into(),
        state: "Mining".into(),
        generated_at: chrono::Utc::now(),
        watching: None,
        eligible: vec![],
        rejected: vec![],
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

#[tokio::test]
async fn health_reports_ok() {
    let base = start(Hub::new(), None).await;
    let body: serde_json::Value = reqwest::get(format!("{base}/api/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
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
