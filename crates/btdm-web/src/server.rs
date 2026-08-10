//! The axum server.
//!
//! Assets are compiled in, so `cargo build` alone produces the finished binary
//! and there is no JavaScript toolchain in the build.

use crate::hub::Hub;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
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
    let state = AppState { hub, token };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/snapshots", get(snapshots))
        .route("/api/events", get(events))
        .route("/api/health", get(health))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| ServeError::Bind {
            addr: addr.to_string(),
            source,
        })?;

    tracing::info!(%addr, "dashboard listening");
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

async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !authorised(&state, &headers, uri.query()) {
        return unauthorised();
    }
    Html(INDEX_HTML).into_response()
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
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

async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !authorised(&state, &headers, uri.query()) {
        return unauthorised();
    }

    let rx = state.hub.subscribe();
    let stream = event_stream(rx);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn event_stream(
    rx: tokio::sync::broadcast::Receiver<crate::hub::AccountEvent>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|item| {
        // A lagging subscriber drops events rather than killing the stream;
        // the snapshot poll fills any gap.
        let item = item.ok()?;
        let payload = serde_json::json!({
            "account": item.account,
            "at": item.event.at,
            "kind": format!("{:?}", item.event.kind),
        });
        Some(Ok(Event::default().data(payload.to_string())))
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
        assert!(
            !INDEX_HTML.contains("http://") && !INDEX_HTML.contains("https://"),
            "the page must not reference external resources"
        );
    }
}
