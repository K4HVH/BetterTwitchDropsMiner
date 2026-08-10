//! GQL transport.
//!
//! Twitch's client uses persisted queries identified by a SHA-256 hash. Those
//! hashes change without notice; the reference implementation ships hardcoded
//! ones and breaks quietly when they rot — several of its commits are nothing
//! but hash updates. Here every operation also carries its raw query text, so
//! a `PersistedQueryNotFound` triggers an immediate retry rather than a
//! failure the user only notices as "nothing is happening".

use crate::TwitchError;
use crate::client::{CLIENT_ID, GQL_URL};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy)]
pub struct GqlOperation {
    pub name: &'static str,
    pub sha256: &'static str,
    /// Must be valid standalone GraphQL: every variable it declares has to be
    /// referenced in the body, or Twitch rejects the fallback outright.
    pub raw_query: &'static str,
}

pub struct GqlClient {
    http: reqwest::Client,
    token: Option<String>,
    endpoint: String,
}

impl GqlClient {
    pub fn new(http: reqwest::Client, token: Option<String>) -> Self {
        Self::with_endpoint(http, token, GQL_URL.to_owned())
    }

    /// Test seam: point the client at a mock server.
    pub fn with_endpoint(http: reqwest::Client, token: Option<String>, endpoint: String) -> Self {
        Self {
            http,
            token,
            endpoint,
        }
    }

    /// Runs `op`, returning its `data` object.
    pub async fn run(&self, op: &GqlOperation, variables: Value) -> Result<Value, TwitchError> {
        let persisted = json!({
            "operationName": op.name,
            "variables": variables,
            "extensions": { "persistedQuery": { "version": 1, "sha256Hash": op.sha256 } },
        });

        match self.post(&persisted).await {
            Err(TwitchError::StaleQueryHash) => {
                tracing::warn!(
                    operation = op.name,
                    "persisted query hash is stale; falling back to the raw query"
                );
                let raw = json!({
                    "operationName": op.name,
                    "variables": variables,
                    "query": op.raw_query,
                });
                self.post(&raw).await
            }
            other => other,
        }
    }

    async fn post(&self, body: &Value) -> Result<Value, TwitchError> {
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("Client-Id", CLIENT_ID)
            .json(body);
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("OAuth {token}"));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        match resp.status().as_u16() {
            200 => {}
            401 | 403 => return Err(TwitchError::Unauthorized),
            429 => return Err(TwitchError::RateLimited),
            s => return Err(TwitchError::Network(format!("HTTP {s}"))),
        }

        let json: Value = resp
            .json()
            .await
            .map_err(|e| TwitchError::Malformed(e.to_string()))?;

        if let Some(errors) = json.get("errors").and_then(Value::as_array)
            && !errors.is_empty()
        {
            let stale = errors.iter().any(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.contains("PersistedQueryNotFound"))
            });
            return Err(if stale {
                TwitchError::StaleQueryHash
            } else {
                TwitchError::Gql(errors.to_vec())
            });
        }

        json.get("data")
            .filter(|v| !v.is_null())
            .cloned()
            .ok_or_else(|| TwitchError::Malformed("response had no `data`".into()))
    }
}
