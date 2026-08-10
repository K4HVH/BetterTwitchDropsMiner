//! Twitch API client: auth, GQL, spade heartbeat, PubSub.

pub mod api_impl;
pub mod auth;
pub mod client;
pub mod gql;
pub mod operations;
pub mod parse;
pub mod pubsub;
pub mod watch;

#[derive(Debug, thiserror::Error)]
pub enum TwitchError {
    #[error("network failure: {0}")]
    Network(String),
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
            other => ApiError::Malformed(other.to_string()),
        }
    }
}
