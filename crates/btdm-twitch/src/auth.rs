//! OAuth 2.0 device-code flow.
//!
//! The user is shown an eight-character code to enter at twitch.tv/activate.
//! No browser automation and no cookie jar, which is what makes this workable
//! on a headless machine.

use crate::TwitchError;
use crate::client::{CLIENT_ID, CLIENT_URL};
use serde::Deserialize;

pub const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
pub const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
pub const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
pub const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval_secs: u64,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// The user has not entered the code yet. Wait `interval_secs` and retry.
    Pending,
    Granted {
        access_token: String,
        refresh_token: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedToken {
    pub user_id: String,
    pub login: String,
    /// Seconds until Twitch stops accepting this token.
    pub expires_in_secs: u64,
}

/// Decides whether a failed token poll means "keep waiting" or "give up now".
///
/// Twitch's OAuth error strings arrive either as `{"message": "..."}` or the
/// RFC-standard `{"error": "..."}`, so both are matched.
fn classify_poll_failure(status: u16, body: &str) -> Result<PollOutcome, TwitchError> {
    let lower = body.to_ascii_lowercase();

    // Still waiting on the human. `slow_down` also means keep going.
    if lower.contains("authorization_pending") || lower.contains("slow_down") {
        return Ok(PollOutcome::Pending);
    }

    // Terminal: no amount of waiting fixes these.
    for terminal in [
        "expired_token",
        "access_denied",
        "invalid_client",
        "invalid_grant",
        "unauthorized_client",
    ] {
        if lower.contains(terminal) {
            return Err(TwitchError::Malformed(format!(
                "device authorisation failed: {terminal}"
            )));
        }
    }

    // An unrecognised 400 is most likely a pending variant, so keep waiting but
    // leave a trail. Anything else is a genuine transport problem.
    if status == 400 {
        tracing::debug!(status, body, "unrecognised token-poll response; still waiting");
        return Ok(PollOutcome::Pending);
    }
    if status == 429 {
        return Err(TwitchError::RateLimited);
    }
    Err(TwitchError::Network(format!("HTTP {status}: {body}")))
}

pub struct AuthClient {
    http: reqwest::Client,
    device_id: String,
    device_url: String,
    token_url: String,
    validate_url: String,
}

impl AuthClient {
    pub fn new(http: reqwest::Client, device_id: String) -> Self {
        Self {
            http,
            device_id,
            device_url: crate::endpoints::device(),
            token_url: crate::endpoints::token(),
            validate_url: crate::endpoints::validate(),
        }
    }

    /// Test seam: point the flow at a mock server.
    pub fn with_endpoints(
        http: reqwest::Client,
        device_id: String,
        device_url: String,
        token_url: String,
        validate_url: String,
    ) -> Self {
        Self {
            http,
            device_id,
            device_url,
            token_url,
            validate_url,
        }
    }

    fn auth_headers(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .header("Accept", "application/json")
            .header("Accept-Language", "en-US")
            .header("Client-Id", CLIENT_ID)
            .header("Origin", CLIENT_URL)
            .header("Referer", CLIENT_URL)
            .header("X-Device-Id", &self.device_id)
    }

    pub async fn start_device_flow(&self) -> Result<DeviceCode, TwitchError> {
        #[derive(Deserialize)]
        struct Resp {
            device_code: String,
            user_code: String,
            verification_uri: String,
            interval: u64,
            expires_in: u64,
        }

        let req = self
            .auth_headers(self.http.post(&self.device_url))
            .form(&[("client_id", CLIENT_ID), ("scopes", "")]);

        let resp = req
            .send()
            .await
            .map_err(|e| TwitchError::Network(crate::describe(&e)))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TwitchError::Network(format!("HTTP {status}: {body}")));
        }

        let r: Resp = resp
            .json()
            .await
            .map_err(|e| TwitchError::Malformed(crate::describe(&e)))?;
        Ok(DeviceCode {
            device_code: r.device_code,
            user_code: r.user_code,
            verification_uri: r.verification_uri,
            interval_secs: r.interval,
            expires_in_secs: r.expires_in,
        })
    }

    /// One poll.
    ///
    /// Twitch answers 400 with `authorization_pending` until the user enters
    /// the code, which is normal. It also answers 400 for terminal failures
    /// like `invalid_client` or `access_denied`. Treating every 400 as "keep
    /// waiting" — as the reference implementation does — means a flow that was
    /// broken from the first request polls silently for the full 30 minutes
    /// and then blames the user for being too slow. Terminal errors are
    /// surfaced immediately instead.
    pub async fn poll_once(&self, dc: &DeviceCode) -> Result<PollOutcome, TwitchError> {
        #[derive(Deserialize)]
        struct Resp {
            access_token: String,
            refresh_token: Option<String>,
        }

        let req = self
            .auth_headers(self.http.post(&self.token_url))
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", &dc.device_code),
                ("grant_type", DEVICE_CODE_GRANT),
            ]);

        let resp = req
            .send()
            .await
            .map_err(|e| TwitchError::Network(crate::describe(&e)))?;

        let status = resp.status().as_u16();
        if status == 200 {
            let r: Resp = resp
                .json()
                .await
                .map_err(|e| TwitchError::Malformed(crate::describe(&e)))?;
            return Ok(PollOutcome::Granted {
                access_token: r.access_token,
                refresh_token: r.refresh_token,
            });
        }

        let body = resp.text().await.unwrap_or_default();
        classify_poll_failure(status, &body)
    }

    pub async fn validate(&self, token: &str) -> Result<ValidatedToken, TwitchError> {
        #[derive(Deserialize)]
        struct Resp {
            user_id: String,
            login: String,
            #[serde(default)]
            expires_in: u64,
        }

        let resp = self
            .http
            .get(&self.validate_url)
            .header("Authorization", format!("OAuth {token}"))
            .send()
            .await
            .map_err(|e| TwitchError::Network(crate::describe(&e)))?;

        match resp.status().as_u16() {
            200 => {
                let r: Resp = resp
                    .json()
                    .await
                    .map_err(|e| TwitchError::Malformed(crate::describe(&e)))?;
                Ok(ValidatedToken {
                    user_id: r.user_id,
                    login: r.login,
                    expires_in_secs: r.expires_in,
                })
            }
            401 | 403 => Err(TwitchError::Unauthorized),
            s => Err(TwitchError::Network(format!("HTTP {s}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_authorisation_keeps_polling() {
        assert_eq!(
            classify_poll_failure(400, r#"{"message":"authorization_pending"}"#).unwrap(),
            PollOutcome::Pending
        );
        assert_eq!(
            classify_poll_failure(400, r#"{"error":"slow_down"}"#).unwrap(),
            PollOutcome::Pending
        );
    }

    /// Polling for 30 minutes and then reporting "you were too slow" when the
    /// client ID was rejected on the very first request is a misleading
    /// failure, not a slow one.
    #[test]
    fn terminal_oauth_errors_stop_immediately() {
        for terminal in [
            "expired_token",
            "access_denied",
            "invalid_client",
            "invalid_grant",
            "unauthorized_client",
        ] {
            let body = format!(r#"{{"message":"{terminal}"}}"#);
            let err = classify_poll_failure(400, &body).unwrap_err();
            assert!(
                err.to_string().contains(terminal),
                "{terminal} must be surfaced verbatim, got: {err}"
            );
        }
    }

    #[test]
    fn an_unrecognised_400_keeps_waiting_rather_than_aborting_a_valid_login() {
        assert_eq!(
            classify_poll_failure(400, r#"{"message":"something new"}"#).unwrap(),
            PollOutcome::Pending
        );
    }

    #[test]
    fn transport_failures_are_not_mistaken_for_pending() {
        assert!(matches!(
            classify_poll_failure(429, ""),
            Err(TwitchError::RateLimited)
        ));
        assert!(matches!(
            classify_poll_failure(503, "gateway down"),
            Err(TwitchError::Network(_))
        ));
    }
}
