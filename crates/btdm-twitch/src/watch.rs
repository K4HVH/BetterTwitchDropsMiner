//! The watch heartbeat.
//!
//! Twitch counts drop time from "minute-watched" analytics events posted to a
//! per-deployment "spade" endpoint. That URL is in no API — it is embedded in
//! the site's JavaScript config and has to be scraped. The mobile page exposes
//! it inline, so we fetch that first and skip a hop.

use crate::TwitchError;
use base64::Engine;
use btdm_core::model::{Channel, Stream, UserId};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;
use std::sync::LazyLock;

pub const MOBILE_BASE_URL: &str = "https://m.twitch.tv";

static SPADE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#""spade_?[Uu]rl"\s*:\s*"(https://[.\w\-/]+)""#).expect("valid regex")
});

static SETTINGS_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"src="(https://[\w.\-]+/config/settings\.[0-9a-f]{32}\.js)""#)
        .expect("valid regex")
});

pub fn extract_spade_url(html: &str) -> Option<String> {
    SPADE_RE.captures(html).map(|c| c[1].to_owned())
}

pub fn extract_settings_url(html: &str) -> Option<String> {
    SETTINGS_RE.captures(html).map(|c| c[1].to_owned())
}

/// The base64-encoded analytics payload Twitch expects in the `data` form field.
///
/// `client_time` is an ISO-8601 string with millisecond precision, matching the
/// real client. Sending a Unix timestamp here is accepted by the transport and
/// then silently ignored by the analytics pipeline — heartbeats succeed and no
/// drop time accrues, which is the exact invisible failure this project exists
/// to prevent.
pub fn build_watch_payload(
    channel: &Channel,
    stream: &Stream,
    user_id: &UserId,
    now: DateTime<Utc>,
) -> String {
    let (game_name, game_id) = match &stream.game {
        Some(g) => (g.name.clone(), g.id.0.clone()),
        None => (String::new(), String::new()),
    };

    let payload = json!([{
        "event": "minute-watched",
        "properties": {
            "broadcast_id": stream.broadcast_id,
            "channel_id": channel.id.0,
            "channel": channel.login,
            "client_time": now.to_rfc3339_opts(SecondsFormat::Millis, true),
            "game": game_name,
            "game_id": game_id,
            "hidden": false,
            "is_live": true,
            "live": true,
            "logged_in": true,
            "minutes_logged": 1,
            "muted": false,
            "user_id": user_id.0,
        }
    }]);

    base64::engine::general_purpose::STANDARD.encode(payload.to_string())
}

/// Turns a transport failure against the spade host into an actionable message.
///
/// `spade.twitch.tv` is an analytics domain, so it appears on most ad-blocking
/// and tracker blocklists — and it is also the *only* endpoint that advances
/// drop time. A blocked spade host therefore produces a miner that looks
/// perfectly healthy and earns nothing, which is the exact failure this project
/// exists to prevent. A bare "error sending request" would bury it, so the
/// likely cause is named outright.
pub fn classify_spade_transport_error(url: &str, e: &reqwest::Error) -> TwitchError {
    if e.is_connect() || e.is_timeout() {
        TwitchError::SpadeUnreachable {
            url: url.to_owned(),
            detail: e.to_string(),
        }
    } else {
        TwitchError::Network(e.to_string())
    }
}

pub struct WatchClient {
    http: reqwest::Client,
}

impl WatchClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    /// Walks the chain: channel page -> (settings JS) -> spade URL.
    pub async fn discover_spade_url(&self, channel_login: &str) -> Result<String, TwitchError> {
        let page_url = format!("{MOBILE_BASE_URL}/{channel_login}");
        let html = self
            .http
            .get(&page_url)
            .send()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?
            .text()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        if let Some(url) = extract_spade_url(&html) {
            return Ok(url);
        }

        let settings_url = extract_settings_url(&html).ok_or_else(|| {
            TwitchError::Malformed(
                "neither a spade URL nor a settings script was found on the channel page".into(),
            )
        })?;
        let js = self
            .http
            .get(&settings_url)
            .send()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?
            .text()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        extract_spade_url(&js)
            .ok_or_else(|| TwitchError::Malformed("no spade URL in the settings script".into()))
    }

    /// One heartbeat. Twitch answers 204 on success.
    pub async fn send(
        &self,
        spade_url: &str,
        channel: &Channel,
        stream: &Stream,
        user_id: &UserId,
    ) -> Result<(), TwitchError> {
        let data = build_watch_payload(channel, stream, user_id, Utc::now());
        let resp = self
            .http
            .post(spade_url)
            .form(&[("data", data)])
            .send()
            .await
            .map_err(|e| classify_spade_transport_error(spade_url, &e))?;

        match resp.status().as_u16() {
            200 | 204 => Ok(()),
            401 | 403 => Err(TwitchError::Unauthorized),
            429 => Err(TwitchError::RateLimited),
            s => Err(TwitchError::Network(format!("spade returned HTTP {s}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btdm_core::model::{ChannelId, Game, GameId};

    fn channel() -> Channel {
        Channel {
            id: ChannelId::from("12345"),
            login: "streamer1".into(),
            display_name: "Streamer1".into(),
        }
    }

    fn stream(game: Option<Game>) -> Stream {
        Stream {
            broadcast_id: "bc-99".into(),
            game,
            title: "t".into(),
            viewers: 10,
            drops_enabled: true,
        }
    }

    fn decode(encoded: &str) -> serde_json::Value {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64");
        serde_json::from_slice(&bytes).expect("valid JSON")
    }

    #[test]
    fn finds_a_spade_url_embedded_directly_in_the_page() {
        let html = r#"<script>var x = {"spade_url":"https://video-edge-1.pdx.abs.hls.ttvnw.net/v1/event"};</script>"#;
        assert_eq!(
            extract_spade_url(html).as_deref(),
            Some("https://video-edge-1.pdx.abs.hls.ttvnw.net/v1/event")
        );
    }

    #[test]
    fn tolerates_the_spaced_and_underscore_free_spellings() {
        assert!(extract_spade_url(r#"{"spadeUrl": "https://a.example/v1/e"}"#).is_some());
        assert!(extract_spade_url(r#"{"spade_url" : "https://a.example/v1/e"}"#).is_some());
    }

    #[test]
    fn finds_the_settings_js_url_when_the_page_lacks_the_spade_url() {
        let html = r#"<script src="https://static.twitchcdn.net/config/settings.0123456789abcdef0123456789abcdef.js"></script>"#;
        assert!(extract_spade_url(html).is_none());
        assert_eq!(
            extract_settings_url(html).as_deref(),
            Some("https://static.twitchcdn.net/config/settings.0123456789abcdef0123456789abcdef.js")
        );
    }

    #[test]
    fn watch_payload_carries_the_fields_twitch_counts_on() {
        let now = DateTime::parse_from_rfc3339("2026-08-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let game = Game {
            id: GameId::from("g1"),
            name: "Deadlock".into(),
            slug: "deadlock".into(),
        box_art_url: None,
        };

        let parsed = decode(&build_watch_payload(
            &channel(),
            &stream(Some(game)),
            &UserId::from("999"),
            now,
        ));

        assert_eq!(parsed[0]["event"], "minute-watched");
        let p = &parsed[0]["properties"];
        assert_eq!(p["channel_id"], "12345");
        assert_eq!(p["channel"], "streamer1");
        assert_eq!(p["broadcast_id"], "bc-99", "a stale broadcast id earns nothing");
        assert_eq!(p["game"], "Deadlock");
        assert_eq!(p["game_id"], "g1");
        assert_eq!(p["user_id"], "999");
        assert_eq!(p["live"], true);
        assert_eq!(p["logged_in"], true);
        assert_eq!(p["minutes_logged"], 1);
    }

    /// The real client sends an ISO-8601 millisecond string. A Unix timestamp
    /// is accepted by the endpoint and then ignored, producing successful
    /// heartbeats that earn nothing.
    #[test]
    fn client_time_is_an_iso8601_millisecond_string() {
        let now = DateTime::parse_from_rfc3339("2026-08-10T12:00:00.123Z")
            .unwrap()
            .with_timezone(&Utc);
        let parsed = decode(&build_watch_payload(
            &channel(),
            &stream(None),
            &UserId::from("999"),
            now,
        ));
        assert_eq!(parsed[0]["properties"]["client_time"], "2026-08-10T12:00:00.123Z");
    }

    #[test]
    fn watch_payload_handles_a_stream_with_no_game() {
        let parsed = decode(&build_watch_payload(
            &channel(),
            &stream(None),
            &UserId::from("9"),
            Utc::now(),
        ));
        assert_eq!(parsed[0]["properties"]["game"], "");
        assert_eq!(parsed[0]["properties"]["game_id"], "");
    }
}
