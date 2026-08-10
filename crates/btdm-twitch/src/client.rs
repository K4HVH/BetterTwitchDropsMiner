//! Shared HTTP setup.
//!
//! Every request presents as the Twitch Android app, because that is the client
//! the device-code auth endpoint accepts. The web client ID is rejected there.

pub const CLIENT_ID: &str = "kd1unb4b3q4t58fwlpcbzcbnm76a8fp";
pub const USER_AGENT: &str = "Dalvik/2.1.0 (Linux; U; Android 16; SM-S911B Build/TP1A.220624.014) tv.twitch.android.app/25.3.0/2503006";
pub const GQL_URL: &str = "https://gql.twitch.tv/gql";
pub const CLIENT_URL: &str = "https://www.twitch.tv";

/// A random 32-character lowercase hex device ID.
///
/// Twitch expects an `X-Device-Id` on auth requests. It only has to be stable
/// for a login session and unique per install, so it is generated once and
/// persisted alongside the tokens.
/// 128 bits of OS entropy as lowercase hex.
///
/// `RandomState::hash_one` was used here originally. It is seeded to resist
/// hash flooding, not to produce secrets: the real entropy is far below the
/// 128 bits the string length implies. Anything guarding a live session has to
/// come from the OS.
pub fn random_hex_128() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS must be able to provide entropy");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn generate_device_id() -> String {
    random_hex_128()
}

pub fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .gzip(true)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_are_32_hex_chars() {
        let id = generate_device_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn device_ids_differ_between_calls() {
        assert_ne!(generate_device_id(), generate_device_id());
    }
}
