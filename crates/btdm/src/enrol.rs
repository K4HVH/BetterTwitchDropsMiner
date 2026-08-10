//! Logging in from the dashboard.
//!
//! A dead or missing token used to need shell access to fix, which is a poor
//! answer on the headless box this project targets — and worse, the miner had
//! already exited by then, so there was nothing left to ask.
//!
//! The device-code flow needs no browser on the machine running the miner:
//! Twitch shows a short code, the user enters it anywhere, and the miner
//! collects the token. That works exactly as well over HTTP as it does on a
//! terminal, so it is offered both ways.

use crate::storage::{Storage, Tokens};
use btdm_twitch::auth::{AuthClient, PollOutcome};
use btdm_twitch::client::{build_http_client, generate_device_id};
use btdm_web::{Enrolment, Hub, LoginState};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Enroller {
    storage: Arc<Storage>,
    hub: Hub,
    /// Tells the supervisor to start mining an account that has just been
    /// authorised, so a re-login does not need a restart.
    start: tokio::sync::mpsc::Sender<String>,
    /// Logins already running, so a user clicking twice does not start two
    /// device flows and invalidate the code they are looking at.
    in_flight: Mutex<HashSet<String>>,
}

impl Enroller {
    pub fn new(
        storage: Arc<Storage>,
        hub: Hub,
        start: tokio::sync::mpsc::Sender<String>,
    ) -> Self {
        Self {
            storage,
            hub,
            start,
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    fn report(&self, account: &str, status: &str, detail: Option<String>) {
        self.hub.set_login_state(LoginState {
            account: account.to_owned(),
            status: status.to_owned(),
            user_code: None,
            verification_uri: None,
            detail,
        });
    }
}

impl Enrolment for Enroller {
    fn begin(&self, account: &str) -> Result<(), String> {
        let account = account.trim().to_owned();
        if account.is_empty() {
            return Err("an account name is required".into());
        }

        {
            let mut guard = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            if !guard.insert(account.clone()) {
                return Err(format!("a login for '{account}' is already in progress"));
            }
        }

        let storage = Arc::clone(&self.storage);
        let hub = self.hub.clone();
        let start = self.start.clone();
        let in_flight_account = account.clone();

        // The device flow is minutes of polling; an HTTP handler cannot hold
        // that open, so it runs on its own and reports through the hub.
        tokio::spawn(async move {
            let outcome = run_login(&storage, &hub, &account).await;

            match outcome {
                Ok(login) => {
                    hub.set_login_state(LoginState {
                        account: account.clone(),
                        status: "granted".into(),
                        user_code: None,
                        verification_uri: None,
                        detail: Some(format!("Linked to Twitch user {login}")),
                    });
                    // Ask the supervisor to mine it. A failure here means the
                    // supervisor is gone, i.e. we are shutting down.
                    let _ = start.send(account.clone()).await;
                }
                Err(e) => {
                    tracing::warn!(account = %account, error = %e, "dashboard login failed");
                    hub.set_login_state(LoginState {
                        account: account.clone(),
                        status: "failed".into(),
                        user_code: None,
                        verification_uri: None,
                        detail: Some(e),
                    });
                }
            }
        });

        self.report(&in_flight_account, "starting", None);
        Ok(())
    }
}

/// Runs one device-code flow to completion, publishing the code as soon as
/// Twitch issues it. Returns the Twitch login on success.
async fn run_login(storage: &Storage, hub: &Hub, account: &str) -> Result<String, String> {
    // Reuse the device ID across re-logins so Twitch sees a stable device.
    let device_id = storage
        .get_tokens(account)
        .ok()
        .flatten()
        .map(|t| t.device_id)
        .unwrap_or_else(generate_device_id);

    let http = build_http_client().map_err(|e| e.to_string())?;
    let auth = AuthClient::new(http, device_id.clone());

    let dc = auth
        .start_device_flow()
        .await
        .map_err(|e| format!("Twitch would not issue a code: {e}"))?;

    hub.set_login_state(LoginState {
        account: account.to_owned(),
        status: "pending".into(),
        user_code: Some(dc.user_code.clone()),
        verification_uri: Some(dc.verification_uri.clone()),
        detail: None,
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(dc.expires_in_secs);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("the code expired before it was entered".into());
        }
        tokio::time::sleep(Duration::from_secs(dc.interval_secs.max(1))).await;

        match auth.poll_once(&dc).await {
            Ok(PollOutcome::Pending) => continue,
            Ok(PollOutcome::Granted {
                access_token,
                refresh_token,
            }) => {
                let validated = auth
                    .validate(&access_token)
                    .await
                    .map_err(|e| format!("the new token did not validate: {e}"))?;

                storage
                    .put_tokens(
                        account,
                        &Tokens {
                            access_token,
                            refresh_token,
                            user_id: validated.user_id.clone(),
                            login: validated.login.clone(),
                            device_id,
                        },
                    )
                    .map_err(|e| format!("could not store the token: {e}"))?;

                tracing::info!(account, login = %validated.login, "authorised from the dashboard");
                return Ok(validated.login);
            }
            // A terminal OAuth error — the user denied it, or the code is
            // gone. Retrying would poll a dead code until it expired.
            Err(e) => return Err(e.to_string()),
        }
    }
}
