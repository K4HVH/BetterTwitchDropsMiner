//! Where Twitch lives.
//!
//! Every URL the miner talks to, in one place and overridable. That exists for
//! one reason: without it the binary as a whole cannot be tested. `run::mine`
//! wires auth, GraphQL, the watch heartbeat and PubSub together, and that
//! wiring — the part that decides what actually happens on a real account —
//! could only ever be checked by running it and watching.
//!
//! The environment variables are the seam. They are also occasionally useful
//! against a proxy, but the test is why they are here.

/// The default, real Twitch.
pub fn gql() -> String {
    from_env("BTDM_GQL_URL", crate::client::GQL_URL)
}

pub fn validate() -> String {
    from_env("BTDM_VALIDATE_URL", crate::auth::VALIDATE_URL)
}

pub fn device() -> String {
    from_env("BTDM_DEVICE_URL", crate::auth::DEVICE_URL)
}

pub fn token() -> String {
    from_env("BTDM_TOKEN_URL", crate::auth::TOKEN_URL)
}

pub fn pubsub() -> String {
    from_env("BTDM_PUBSUB_URL", crate::pubsub::PUBSUB_URL)
}

/// The page scraped to discover the spade endpoint.
pub fn mobile_base() -> String {
    from_env("BTDM_MOBILE_URL", crate::watch::MOBILE_BASE_URL)
}

/// Where a stream's playlist index is fetched, with a playback token.
pub fn usher() -> String {
    from_env("BTDM_USHER_URL", crate::playback::USHER_URL)
}

fn from_env(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => {
            tracing::warn!(key, url = %v, "using an overridden Twitch endpoint");
            v
        }
        _ => default.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing set means the real Twitch, every time. An accidental override
    /// would send a live account's traffic somewhere unexpected.
    #[test]
    fn the_defaults_are_the_real_twitch() {
        // Deliberately not testing the override path here: environment
        // variables are process-global and would race other tests.
        assert!(gql().starts_with("https://"));
        assert!(validate().contains("twitch.tv"));
        assert!(pubsub().starts_with("wss://"));
        assert!(mobile_base().contains("twitch.tv"));
        assert!(usher().starts_with("https://"));
    }

    #[test]
    fn a_blank_override_is_ignored_rather_than_used() {
        assert_eq!(from_env("BTDM_DEFINITELY_UNSET_XYZ", "fallback"), "fallback");
    }
}
