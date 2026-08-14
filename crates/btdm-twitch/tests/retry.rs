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

// ---------------------------------------------------------------------------
// Batching and rate limiting
// ---------------------------------------------------------------------------

/// A selection pass asks about every live channel. One request per channel is
/// both slower and far likelier to be throttled.
#[tokio::test]
async fn availability_checks_are_batched_into_one_request() {
    // Twenty answers in one array, matching the twenty operations sent.
    let body: &'static str = Box::leak(
        format!(
            "[{}]",
            (0..20)
                .map(|_| r#"{"data":{"channel":{"viewerDropCampaigns":[]}}}"#)
                .collect::<Vec<_>>()
                .join(",")
        )
        .into_boxed_str(),
    );

    let (url, hits) = server(vec![], body).await;
    let api = client(url);

    let channels: Vec<btdm_core::model::ChannelId> = (0..20)
        .map(|i| btdm_core::model::ChannelId::from(&format!("ch{i}")[..]))
        .collect();

    let answers = api.available_drops_many(&channels).await;

    assert_eq!(answers.len(), 20, "every channel gets an answer");
    assert!(answers.iter().all(|(_, r)| r.is_ok()));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "twenty channels must be one request, not twenty"
    );
}

/// A batch that fails wholesale must report a failure for each channel in it
/// rather than silently answering "no drops available", which would look like
/// a channel that earns nothing and get it skipped.
#[tokio::test]
async fn a_failed_batch_reports_failure_per_channel() {
    let (url, _) = server(vec![500, 500, 500, 500, 500, 500], "[]").await;
    let api = client(url);

    let channels: Vec<btdm_core::model::ChannelId> =
        (0..3).map(|i| btdm_core::model::ChannelId::from(&format!("ch{i}")[..])).collect();

    let answers = api.available_drops_many(&channels).await;
    assert_eq!(answers.len(), 3);
    assert!(
        answers.iter().all(|(_, r)| r.is_err()),
        "an unanswered check must not read as 'earns nothing'"
    );
}

/// Twitch's GraphQL endpoint punishes bursts. The reference implementation
/// caps at five a second and warns against changing it.
#[tokio::test]
async fn requests_are_rate_limited() {
    let (url, hits) = server(vec![], EMPTY_INVENTORY).await;
    let api = client(url);

    let started = std::time::Instant::now();
    for _ in 0..12 {
        let _ = api.fetch_inventory().await;
    }
    let elapsed = started.elapsed();

    assert_eq!(hits.load(Ordering::SeqCst), 12);
    // Five of burst, then seven more at five a second: at least ~1.4s.
    assert!(
        elapsed.as_millis() >= 1_000,
        "twelve requests went out in {elapsed:?}, which is faster than the cap"
    );
}

/// A server that accepts the connection and then never answers must not park
/// the caller forever.
///
/// The miner polls its command channel only while waiting between heartbeats,
/// never during a sync or a selection pass. So a request that never returns
/// stopped everything: no mining, no commands, no Ctrl-C. reqwest has no
/// default timeout, so this had to be asked for.
#[tokio::test]
async fn a_request_that_never_answers_eventually_gives_up() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Accept, then hold the socket open saying nothing at all.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let client = btdm_twitch::client::build_http_client().unwrap();
    let started = std::time::Instant::now();
    let result = client.get(format!("http://{addr}/")).send().await;

    assert!(result.is_err(), "a silent server must not be waited on forever");
    assert!(
        started.elapsed() < btdm_twitch::client::REQUEST_TIMEOUT + std::time::Duration::from_secs(5),
        "it waited {:?}, past its own timeout",
        started.elapsed()
    );
}
