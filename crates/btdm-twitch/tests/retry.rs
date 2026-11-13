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
        endpoint,
    )
}

const EMPTY_INVENTORY: &str =
    r#"{"data":{"currentUser":{"inventory":{"dropCampaignsInProgress":[]}}}}"#;

/// A server that promises more body than it delivers on its first `broken`
/// replies, then answers properly. Announcing a `Content-Length` and then
/// closing the socket early is what a connection dropped mid-download looks
/// like from the client side.
async fn truncating_server(broken: usize, body: &'static str) -> (String, Arc<AtomicUsize>) {
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

            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;

            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(headers.as_bytes()).await;
            let payload = if n < broken {
                // Well short of the length just promised.
                &body[..body.len() / 3]
            } else {
                body
            };
            let _ = socket.write_all(payload.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });

    (format!("http://{addr}/gql"), hits)
}

/// A server that answers 200 with something that is not JSON at all.
async fn not_json_server(body: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });

    (format!("http://{addr}/gql"), hits)
}

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

// ---------------------------------------------------------------------------
// Telling a body that never arrived apart from a body that is not JSON
// ---------------------------------------------------------------------------

/// Observed live on 2026-08-27: sixteen campaigns dropped out of one sync,
/// every one of them reporting "unexpected response shape: unexpected response
/// shape: error decoding response body for url (https://gql.twitch.tv/gql)".
///
/// Both halves of that line were wrong. The response shape was never in
/// question — the body simply stopped arriving — and a failure to *receive* is
/// as transient as the dropped connection that caused it. Filing it as
/// malformed meant it was never retried, so a single blip mid-download cost a
/// whole batch of campaigns until the next sync ten minutes later.
#[tokio::test]
async fn a_body_that_stops_arriving_is_retried() {
    let (url, hits) = truncating_server(1, EMPTY_INVENTORY).await;
    let api = client(url);

    let result = api.fetch_inventory().await;

    assert!(result.is_ok(), "a truncated body must be asked for again: {result:?}");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "it should have asked twice");
}

/// The other side of the same coin. JSON that is not JSON will not become JSON
/// by asking again, and three extra requests into a rate limit is the worst
/// possible response to it.
#[tokio::test]
async fn a_body_that_is_not_json_is_not_retried() {
    let (url, hits) = not_json_server("<html>maintenance</html>").await;
    let api = client(url);

    assert!(matches!(
        api.fetch_inventory().await,
        Err(btdm_core::api::ApiError::Malformed(_))
    ));
    assert_eq!(hits.load(Ordering::SeqCst), 1, "asked exactly once");
}

/// The error a user actually reads. It said "unexpected response shape"
/// twice — `ApiError::Malformed` adding the prefix that `TwitchError::Malformed`
/// had already rendered in — and then stopped at reqwest's own frame, which is
/// the same twelve words for every way a body can fail. Neither half told
/// anyone anything.
#[tokio::test]
async fn a_malformed_body_says_what_was_wrong_with_it_exactly_once() {
    let (url, _) = not_json_server("<html>maintenance</html>").await;
    let api = client(url);

    let Err(e) = api.fetch_inventory().await else {
        panic!("html is not a valid response");
    };
    let message = e.to_string();

    assert_eq!(
        message.matches("unexpected response shape").count(),
        1,
        "the prefix belongs to one layer, not two: {message}"
    );
    assert!(
        message.contains("expected value"),
        "serde knows precisely what was wrong; the chain must carry it: {message}"
    );
}
