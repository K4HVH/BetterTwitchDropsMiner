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
            device_url: DEVICE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
            validate_url: VALIDATE_URL.to_owned(),
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
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TwitchError::Network(format!("HTTP {status}: {body}")));
        }

        let r: Resp = resp
            .json()
            .await
            .map_err(|e| TwitchError::Malformed(e.to_string()))?;
        Ok(DeviceCode {
            device_code: r.device_code,
            user_code: r.user_code,
            verification_uri: r.verification_uri,
            interval_secs: r.interval,
            expires_in_secs: r.expires_in,
        })
    }

    /// One poll. Twitch answers 400 until the user authorises, which is normal
    /// and must not be treated as an error.
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
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        if resp.status().as_u16() == 200 {
            let r: Resp = resp
                .json()
                .await
                .map_err(|e| TwitchError::Malformed(e.to_string()))?;
            return Ok(PollOutcome::Granted {
                access_token: r.access_token,
                refresh_token: r.refresh_token,
            });
        }
        Ok(PollOutcome::Pending)
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
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        match resp.status().as_u16() {
            200 => {
                let r: Resp = resp
                    .json()
                    .await
                    .map_err(|e| TwitchError::Malformed(e.to_string()))?;
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
