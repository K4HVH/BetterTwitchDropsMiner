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
use std::time::Duration;

/// How many times a transient failure is retried before giving up.
const MAX_RETRIES: usize = 3;
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(400);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(4);

/// Whether asking again could plausibly succeed.
fn is_transient(e: &TwitchError) -> bool {
    matches!(e, TwitchError::Transient(_))
}

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
    limiter: RateLimiter,
}

/// A token bucket over the whole client.
///
/// Twitch's GraphQL endpoint is, in the reference implementation's own words,
/// "pretty volatile and breaks everything if one runs into their rate limit".
/// A selection pass can fire well over a hundred requests, so this is the
/// difference between being fast and being throttled.
#[derive(Debug)]
struct RateLimiter {
    inner: tokio::sync::Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: tokio::time::Instant,
}

/// Requests per second. The reference implementation uses the same figure and
/// warns against changing it.
const RATE_PER_SECOND: f64 = 5.0;
/// How many may be spent at once before the rate applies.
const BURST: f64 = 5.0;

impl RateLimiter {
    fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(Bucket {
                tokens: BURST,
                last: tokio::time::Instant::now(),
            }),
        }
    }

    /// Waits until a request may be sent.
    async fn acquire(&self) {
        loop {
            let wait = {
                let mut bucket = self.inner.lock().await;
                let now = tokio::time::Instant::now();
                let elapsed = now.duration_since(bucket.last).as_secs_f64();
                bucket.last = now;
                bucket.tokens = (bucket.tokens + elapsed * RATE_PER_SECOND).min(BURST);

                if bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    return;
                }
                // How long until one token is available.
                Duration::from_secs_f64((1.0 - bucket.tokens) / RATE_PER_SECOND)
            };
            tokio::time::sleep(wait).await;
        }
    }
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
            limiter: RateLimiter::new(),
        }
    }

    /// Runs several operations in one HTTP request.
    ///
    /// Twitch's endpoint accepts an array and answers with one in the same
    /// order. A selection pass that checked 160 channels used to be 160
    /// separate requests; batched, it is eight. Results are per operation, so
    /// one failure does not discard the rest.
    ///
    /// A stale persisted hash is reported per item rather than retried here —
    /// the caller re-runs just that operation through [`GqlClient::run`],
    /// which has the raw-query fallback.
    pub async fn run_many(
        &self,
        ops: &[(&GqlOperation, Value)],
    ) -> Result<Vec<Result<Value, TwitchError>>, TwitchError> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }

        let body: Vec<Value> = ops
            .iter()
            .map(|(op, variables)| {
                json!({
                    "operationName": op.name,
                    "variables": variables,
                    "extensions": {
                        "persistedQuery": { "version": 1, "sha256Hash": op.sha256 }
                    },
                })
            })
            .collect();

        let response = self.post_raw(&Value::Array(body)).await?;
        let array = response
            .as_array()
            .ok_or_else(|| TwitchError::Malformed("batched response was not an array".into()))?;

        if array.len() != ops.len() {
            return Err(TwitchError::Malformed(format!(
                "asked for {} operations and got {} answers",
                ops.len(),
                array.len()
            )));
        }

        Ok(array.iter().map(extract_data).collect())
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

    /// Posts with bounded retry.
    ///
    /// One HTTP 500 or dropped connection used to abort a whole campaign sync
    /// or selection pass, and the miner then sat idle until its next scheduled
    /// one — up to ten minutes of doing nothing because of a single blip. That
    /// is exactly the "random connections just completely failing" symptom
    /// this project exists to fix.
    ///
    /// Only transient failures are retried. A rejected token or a malformed
    /// response will not become valid by asking again, and retrying a rate
    /// limit immediately makes it worse.
    async fn post(&self, body: &Value) -> Result<Value, TwitchError> {
        let mut delay = FIRST_RETRY_DELAY;
        let mut last: TwitchError;
        let mut attempt = 0usize;

        loop {
            match self.post_once(body).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if attempt >= MAX_RETRIES || !is_transient(&e) {
                        return Err(e);
                    }
                    last = e;
                    attempt += 1;
                    tracing::debug!(
                        error = %last,
                        attempt,
                        retry_in_ms = delay.as_millis(),
                        "transient Twitch failure; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_RETRY_DELAY);
                }
            }
        }
    }

    /// A single operation's response, unwrapped to its `data` object.
    async fn post_once(&self, body: &Value) -> Result<Value, TwitchError> {
        extract_data(&self.post_raw_once(body).await?)
    }

    /// As [`GqlClient::post`], but returns the envelope untouched so a batched
    /// array can be split apart by the caller.
    async fn post_raw(&self, body: &Value) -> Result<Value, TwitchError> {
        let mut delay = FIRST_RETRY_DELAY;
        let mut attempt = 0usize;

        loop {
            match self.post_raw_once(body).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if attempt >= MAX_RETRIES || !is_transient(&e) {
                        return Err(e);
                    }
                    attempt += 1;
                    tracing::debug!(error = %e, attempt, "transient failure; retrying batch");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_RETRY_DELAY);
                }
            }
        }
    }

    async fn post_raw_once(&self, body: &Value) -> Result<Value, TwitchError> {
        self.limiter.acquire().await;
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("Client-Id", CLIENT_ID)
            .json(body);
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("OAuth {token}"));
        }

        let resp = req.send().await.map_err(|e| {
            // A connection that never completed is worth another go; anything
            // else is a real problem with the request.
            if e.is_timeout() || e.is_connect() || e.is_request() {
                TwitchError::Transient(e.to_string())
            } else {
                TwitchError::Network(e.to_string())
            }
        })?;

        match resp.status().as_u16() {
            200 => {}
            401 | 403 => return Err(TwitchError::Unauthorized),
            429 => return Err(TwitchError::RateLimited),
            // 5xx is Twitch having a moment; 4xx is us being wrong. Only the
            // former is worth asking again about.
            s @ 500..=599 => return Err(TwitchError::Transient(format!("HTTP {s}"))),
            s => return Err(TwitchError::Network(format!("HTTP {s}"))),
        }

        resp.json()
            .await
            .map_err(|e| TwitchError::Malformed(e.to_string()))
    }
}

/// Pulls `data` out of one GraphQL envelope, turning its `errors` into ours.
fn extract_data(envelope: &Value) -> Result<Value, TwitchError> {
    if let Some(errors) = envelope.get("errors").and_then(Value::as_array)
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

    envelope
        .get("data")
        .filter(|v| !v.is_null())
        .cloned()
        .ok_or_else(|| TwitchError::Malformed("response had no `data`".into()))
}
