//! Twitch API client: auth, GQL, spade heartbeat, PubSub.

pub mod api_impl;
pub mod auth;
pub mod client;
pub mod gql;
pub mod operations;
pub mod endpoints;
pub mod parse;
pub mod pubsub;
pub mod watch;

#[derive(Debug, thiserror::Error)]
pub enum TwitchError {
    #[error("network failure: {0}")]
    Network(String),
    /// A failure that could plausibly succeed if asked again: a 5xx, a
    /// timeout, or a connection that never completed. Retried automatically.
    #[error("temporary failure: {0}")]
    Transient(String),
    #[error("authentication is no longer valid")]
    Unauthorized,
    #[error("rate limited by Twitch")]
    RateLimited,
    #[error("unexpected response shape: {0}")]
    Malformed(String),
    #[error(
        "cannot reach the watch endpoint {url} — drops CANNOT progress while this is \
         blocked. It is an analytics domain, so an ad-blocker, Pi-hole, VPN filter or \
         DNS blocklist is the usual cause: allow-list `spade.twitch.tv`. ({detail})"
    )]
    SpadeUnreachable { url: String, detail: String },
    #[error("persisted query hash is stale")]
    StaleQueryHash,
    #[error("graphql errors: {0:?}")]
    Gql(Vec<serde_json::Value>),
}

impl From<TwitchError> for btdm_core::api::ApiError {
    fn from(e: TwitchError) -> Self {
        use btdm_core::api::ApiError;
        match e {
            TwitchError::Unauthorized => ApiError::Unauthorized,
            TwitchError::RateLimited => ApiError::RateLimited,
            TwitchError::Network(m) => ApiError::Network(m),
            // A transient failure that survived every retry is still a network
            // problem, not a malformed response.
            TwitchError::Transient(m) => ApiError::Network(m),
            // The detail, not the rendered error: `ApiError::Malformed`
            // supplies the same "unexpected response shape" prefix, and
            // passing the rendered form printed it twice. The log read
            // "unexpected response shape: unexpected response shape: error
            // decoding response body", which is one prefix of noise in front
            // of the only three words that carried any information.
            TwitchError::Malformed(m) => ApiError::Malformed(m),
            other => ApiError::Malformed(other.to_string()),
        }
    }
}

/// Every layer of an error, not just the outermost.
///
/// `reqwest::Error`'s own `Display` stops at its own frame, and for a failed
/// body it is the same twelve words whatever went wrong: "error decoding
/// response body for url (https://gql.twitch.tv/gql)". A body that never
/// finished arriving, a connection reset mid-read, a request that ran out of
/// time, and a response that is HTML rather than JSON all print exactly that.
/// Sixteen campaigns were dropped in one sync with that message and nothing in
/// the log could say which of the four had happened, so the chain is walked
/// and joined here rather than thrown away at the point it is needed most.
pub(crate) fn describe(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut source = e.source();
    while let Some(inner) = source {
        let text = inner.to_string();
        // Some wrappers already quote their source verbatim; repeating it
        // would just rebuild the noise this exists to remove.
        if parts.last().is_none_or(|prev| prev != &text) {
            parts.push(text);
        }
        source = inner.source();
    }
    parts.join(": ")
}
