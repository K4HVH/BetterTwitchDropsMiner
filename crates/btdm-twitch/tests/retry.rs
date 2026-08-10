//! Retry behaviour, against a real socket.
//!
//! "Random connections just completely failing" is the symptom this exists to
//! answer: one HTTP 500 used to abort a whole campaign sync, after which the
//! miner idled until its next scheduled one — up to ten minutes of nothing
//! because of a single blip.

use btdm_core::api::TwitchApi;
use btdm_core::model::UserId;
use btdm_twitch::api_impl::TwitchClient;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A one-shot HTTP server that replies with `statuses` in order, then 200
/// forever. Returns its URL and a counter of requests received.
async fn server(statuses: Vec<u16>, body: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let status = statuses.get(n).copied().unwrap_or(200);

            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;

            let payload = if status == 200 { body } else { "{}" };
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });

    (format!("http://{addr}/gql"), hits)
}

fn client(endpoint: String) -> TwitchClient {
    TwitchClient::with_endpoint(
        reqwest::Client::new(),
        "token".into(),
        UserId::from("1"),
        "someone".into(),
        endpoint,
    )
}

const EMPTY_INVENTORY: &str =
    r#"{"data":{"currentUser":{"inventory":{"dropCampaignsInProgress":[]}}}}"#;

#[tokio::test]
async fn a_single_server_error_is_retried_rather_than_failing_the_pass() {
    let (url, hits) = server(vec![500], EMPTY_INVENTORY).await;
    let api = client(url);

    let result = api.fetch_inventory().await;

    assert!(result.is_ok(), "one 500 must not fail the sync: {result:?}");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "it should have asked twice");
}

#[tokio::test]
async fn several_server_errors_in_a_row_are_ridden_out() {
    let (url, hits) = server(vec![500, 502, 503], EMPTY_INVENTORY).await;
    let api = client(url);

    assert!(api.fetch_inventory().await.is_ok());
    assert_eq!(hits.load(Ordering::SeqCst), 4);
}

/// Retrying forever would be worse than failing: the caller has its own
/// schedule and the watchdog is waiting on it.
#[tokio::test]
async fn retries_are_bounded() {
    let (url, hits) = server(vec![500; 20], EMPTY_INVENTORY).await;
    let api = client(url);

    assert!(api.fetch_inventory().await.is_err());
    assert_eq!(
        hits.load(Ordering::SeqCst),
        4,
        "one attempt plus three retries, then give up"
    );
}

/// A rejected token will not become valid by asking again, and hammering it
/// only wastes time the caller could spend telling the user to log in.
#[tokio::test]
async fn authentication_failures_are_not_retried() {
    let (url, hits) = server(vec![401; 5], EMPTY_INVENTORY).await;
    let api = client(url);

    assert!(matches!(
        api.fetch_inventory().await,
        Err(btdm_core::api::ApiError::Unauthorized)
    ));
    assert_eq!(hits.load(Ordering::SeqCst), 1, "asked exactly once");
}

/// Retrying a rate limit immediately makes it worse.
#[tokio::test]
async fn rate_limits_are_not_retried() {
    let (url, hits) = server(vec![429; 5], EMPTY_INVENTORY).await;
    let api = client(url);

    assert!(matches!(
        api.fetch_inventory().await,
        Err(btdm_core::api::ApiError::RateLimited)
    ));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// A 4xx that is not auth means the request itself is wrong; asking again
/// cannot change that.
#[tokio::test]
async fn client_errors_are_not_retried() {
    let (url, hits) = server(vec![400; 5], EMPTY_INVENTORY).await;
    let api = client(url);

    assert!(api.fetch_inventory().await.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
