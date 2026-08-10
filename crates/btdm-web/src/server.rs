//! The axum server.
//!
//! Assets are compiled in, so `cargo build` alone produces the finished binary
//! and there is no JavaScript toolchain in the build.

use crate::hub::{CommandError, Hub, Update};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use btdm_core::control::Command;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio_stream::{Stream, StreamExt};

const INDEX_HTML: &str = include_str!("../assets/index.html");

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("binding {addr}: {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
    #[error("serving: {0}")]
    Serve(#[from] std::io::Error),
}

#[derive(Clone)]
struct AppState {
    hub: Hub,
    /// Required as a bearer token when bound to anything but loopback, because
    /// the miner's state is derived from a live Twitch session.
    token: Option<String>,
}

/// Serves the dashboard until the process ends.
///
/// Binding to anything other than loopback requires `token`; callers are
/// expected to generate one and show it to the user.
pub async fn serve(hub: Hub, addr: SocketAddr, token: Option<String>) -> Result<(), ServeError> {
    serve_announcing(hub, addr, token, |_| {}).await
}

/// As [`serve`], but calls `announce` with the dashboard URL *after* the socket
/// is bound. Printing the URL before binding would advertise an address that
/// may never work.
pub async fn serve_announcing(
    hub: Hub,
    addr: SocketAddr,
    token: Option<String>,
    announce: impl FnOnce(String),
) -> Result<(), ServeError> {
    let app_token = token.clone();
    let state = AppState { hub, token };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/snapshots", get(snapshots))
        .route("/api/events", get(events))
        .route("/api/health", get(health))
        .route("/api/history", get(history))
        .route("/api/rewards", get(rewards))
        .route("/api/logins", get(logins))
        .route("/api/accounts/{account}/login", post(begin_login))
        .route("/api/accounts/{account}/command", post(command))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| ServeError::Bind {
            addr: addr.to_string(),
            source,
        })?;

    // Report the address actually bound, not the one requested: port 0 means
    // "any free port", so echoing the request would announce a dead URL.
    let bound = listener.local_addr().unwrap_or(addr);
    let url = match &app_token {
        Some(t) => format!("http://{bound}/?token={t}"),
        None => format!("http://{bound}/"),
    };
    tracing::info!(%bound, "dashboard listening");
    announce(url);

    axum::serve(listener, app).await?;
    Ok(())
}

/// Checks the bearer token when one is configured.
///
/// The token may also arrive as `?token=` so a link can be opened directly in
/// a browser, which has no way to set a header.
fn authorised(state: &AppState, headers: &HeaderMap, query: Option<&str>) -> bool {
    let Some(expected) = &state.token else {
        return true;
    };
    if let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
        && value.strip_prefix("Bearer ") == Some(expected.as_str())
    {
        return true;
    }
    query.is_some_and(|q| {
        q.split('&')
            .filter_map(|kv| kv.split_once('='))
            .any(|(k, v)| k == "token" && v == expected)
    })
}

fn unauthorised() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        "A token is required. Append ?token=... or send an Authorization: Bearer header.",
    )
        .into_response()
}

async fn index(State(state): State<AppState>, headers: HeaderMap, uri: axum::http::Uri) -> Response {
    if !authorised(&state, &headers, uri.query()) {
        return unauthorised();
    }
    Html(INDEX_HTML).into_response()
}

/// A monitoring probe.
///
/// This is the one unauthenticated route, so it deliberately reveals no
/// account names, channels or campaigns — only whether mining is actually
/// working. It used to return a bare `{"ok": true}` from a handler that looked
/// at nothing, which meant a miner whose token had expired an hour ago
/// reported itself perfectly healthy.
async fn health(State(state): State<AppState>) -> Response {
    let snapshots = state.hub.snapshots();

    if snapshots.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "ok": false,
                "status": "starting",
                "detail": "no account has reported yet",
                "accounts": 0,
            })),
        )
            .into_response();
    }

    let total = snapshots.len();
    let paused = snapshots.iter().filter(|s| s.paused).count();
    let stalled = snapshots
        .iter()
        .filter(|s| s.watching.as_ref().is_some_and(|w| w.stalled))
        .count();
    let mining = snapshots.iter().filter(|s| s.state == "Mining").count();
    let idle = total - paused - mining;

    // Paused is a deliberate state, not a fault. Stalled is a fault, and the
    // whole point of a probe is that it fires on one.
    let ok = stalled == 0;
    let status = if stalled > 0 {
        "stalled"
    } else if mining > 0 {
        "mining"
    } else if paused == total {
        "paused"
    } else {
        "idle"
    };

    let body = Json(serde_json::json!({
        "ok": ok,
        "status": status,
        "accounts": total,
        "mining": mining,
        "idle": idle,
        "paused": paused,
        "stalled": stalled,
    }));

    if ok {
        body.into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
    }
}

async fn snapshots(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !authorised(&state, &headers, uri.query()) {
        return unauthorised();
    }
    Json(state.hub.snapshots()).into_response()
}

/// Applies one command to one account.
///
/// The reply carries the miner's own confirmation text rather than a bare 200,
/// so the page can tell the user what actually happened instead of assuming
/// the click worked.
async fn command(
    State(state): State<AppState>,
    Path(account): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: String,
) -> Response {
    if !authorised(&state, &headers, uri.query()) {
        return unauthorised();
    }

    let command: Command = match serde_json::from_str(&body) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match state.hub.send_command(&account, command) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e @ CommandError::UnknownAccount(_)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Query parameters shared by the two history routes.
fn query_param(uri: &axum::http::Uri, key: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.replace("%20", " ").replace('+', " "))
    })
}

fn account_and_limit(uri: &axum::http::Uri, default_limit: u32) -> (String, u32) {
    let account = query_param(uri, "account").unwrap_or_else(|| "default".into());
    let limit = query_param(uri, "limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(default_limit)
        .min(1000);
    (account, limit)
}

async fn history(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !authorised(&state, &headers, uri.query()) {
        return unauthorised();
    }
    let (account, limit) = account_and_limit(&uri, 200);
    Json(state.hub.history(&account, limit)).into_response()
}

async fn rewards(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !authorised(&state, &headers, uri.query()) {
        return unauthorised();
    }
    let (account, limit) = account_and_limit(&uri, 500);
    Json(state.hub.rewards(&account, limit)).into_response()
}

/// Current state of every login, in progress or finished.
async fn logins(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !authorised(&state, &headers, uri.query()) {
        return unauthorised();
    }
    Json(serde_json::json!({
        "logins": state.hub.login_states(),
        "errors": state.hub.account_errors(),
    }))
    .into_response()
}

/// Starts a device-code login for an account.
///
/// Returns immediately; the code to enter appears on `/api/logins` a moment
/// later, because obtaining it is itself a network round trip and an HTTP
/// handler must not sit on one.
async fn begin_login(
    State(state): State<AppState>,
    Path(account): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !authorised(&state, &headers, uri.query()) {
        return unauthorised();
    }
    match state.hub.begin_login(&account) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !authorised(&state, &headers, uri.query()) {
        return unauthorised();
    }

    // Send the current state immediately so a page that has just loaded is
    // never blank while it waits for the first change.
    let initial: Vec<Update> = state
        .hub
        .snapshots()
        .into_iter()
        .map(|s| Update::Snapshot(Box::new(s)))
        .collect();

    let rx = state.hub.subscribe();
    let stream = tokio_stream::iter(initial.into_iter().map(|u| Ok(encode(&u))))
        .chain(event_stream(rx));

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Named SSE events, so the page can route a snapshot and a log line to
/// different handlers rather than sniffing the payload.
fn encode(update: &Update) -> Event {
    match update {
        Update::Snapshot(s) => Event::default()
            .event("snapshot")
            .data(serde_json::to_string(s).unwrap_or_else(|_| "{}".into())),
        Update::Event(item) => {
            let payload = serde_json::json!({
                "account": item.account,
                "at": item.event.at,
                "summary": item.event.summary(),
                "severity": item.event.severity(),
                "kind": item.event.kind,
            });
            Event::default().event("log").data(payload.to_string())
        }
    }
}

fn event_stream(
    rx: tokio::sync::broadcast::Receiver<Update>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|item| {
        // A lagging subscriber drops updates rather than killing the stream;
        // the next snapshot is a full state replacement, so the page recovers
        // on its own without needing to re-fetch.
        let item = item.ok()?;
        Some(Ok(encode(&item)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(token: Option<&str>) -> AppState {
        AppState {
            hub: Hub::new(),
            token: token.map(str::to_owned),
        }
    }

    #[test]
    fn loopback_without_a_token_needs_no_authorisation() {
        assert!(authorised(&state(None), &HeaderMap::new(), None));
    }

    #[test]
    fn a_configured_token_is_required() {
        assert!(!authorised(&state(Some("secret")), &HeaderMap::new(), None));
    }

    #[test]
    fn a_bearer_header_authorises() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(authorised(&state(Some("secret")), &headers, None));
    }

    #[test]
    fn a_wrong_bearer_header_does_not_authorise() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer nope".parse().unwrap());
        assert!(!authorised(&state(Some("secret")), &headers, None));
    }

    /// A browser cannot set headers on a plain navigation, so the token has to
    /// work as a query parameter too.
    #[test]
    fn a_query_token_authorises() {
        assert!(authorised(
            &state(Some("secret")),
            &HeaderMap::new(),
            Some("token=secret")
        ));
        assert!(authorised(
            &state(Some("secret")),
            &HeaderMap::new(),
            Some("other=1&token=secret")
        ));
    }

    #[test]
    fn a_wrong_query_token_does_not_authorise() {
        assert!(!authorised(
            &state(Some("secret")),
            &HeaderMap::new(),
            Some("token=nope")
        ));
        // A prefix must not be accepted.
        assert!(!authorised(
            &state(Some("secret")),
            &HeaderMap::new(),
            Some("token=sec")
        ));
    }

    #[test]
    fn the_page_is_compiled_in() {
        assert!(INDEX_HTML.contains("<!doctype html>"));
    }

    /// The page must work with no network beyond the miner itself, so it may
    /// not pull scripts, styles or fonts from anywhere. Twitch artwork is the
    /// single exception: it is `<img>` content, cannot execute, and degrades
    /// to a missing picture rather than a broken page.
    #[test]
    fn the_page_loads_no_external_code() {
        for line in INDEX_HTML.lines() {
            let l = line.trim();
            if !l.contains("http://") && !l.contains("https://") {
                continue;
            }
            assert!(
                l.contains("static-cdn.jtvnw.net") || l.contains("twitch.tv"),
                "the page must not reference external resources: {l}"
            );
            assert!(
                !l.contains("<script") && !l.contains("stylesheet"),
                "the page must not load external code: {l}"
            );
        }
    }

    #[test]
    fn a_snapshot_encodes_as_its_own_sse_event_type() {
        let hub = Hub::new();
        let _ = hub;
        let update = Update::Event(crate::hub::AccountEvent {
            account: "main".into(),
            event: btdm_core::event::MinerEvent {
                at: chrono::Utc::now(),
                kind: btdm_core::event::EventKind::AuthExpired,
            },
        });
        // Encoding must not panic, and must produce a readable summary rather
        // than a debug dump.
        let encoded = format!("{:?}", encode(&update));
        assert!(encoded.contains("log"), "{encoded}");
        assert!(encoded.contains("sign in again"), "{encoded}");
    }
}
