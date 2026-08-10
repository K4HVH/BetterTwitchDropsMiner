//! Interactive login and token status.
//!
//! Device-code flow: the user enters a short code at twitch.tv/activate from
//! any device. Nothing here needs a browser on the machine running the miner.

use crate::storage::{Storage, Tokens};
use btdm_twitch::auth::{AuthClient, PollOutcome};
use btdm_twitch::client::{build_http_client, generate_device_id};
use std::time::Duration;

pub async fn login(storage: &Storage, account: &str, force: bool) -> anyhow::Result<()> {
    if !force && let Some(existing) = storage.get_tokens(account)? {
        let http = build_http_client()?;
        let auth = AuthClient::new(http, existing.device_id.clone());
        match auth.validate(&existing.access_token).await {
            Ok(v) => {
                println!(
                    "Account '{account}' is already linked to Twitch user '{}'.\n\
                     Use --force to link a different account.",
                    v.login
                );
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(error = %e, "stored token is no longer valid; logging in again");
            }
        }
    }

    // Reuse the device ID across re-logins so Twitch sees a stable device.
    let device_id = storage
        .get_tokens(account)?
        .map(|t| t.device_id)
        .unwrap_or_else(generate_device_id);

    let http = build_http_client()?;
    let auth = AuthClient::new(http, device_id.clone());

    let dc = auth.start_device_flow().await?;

    println!("\n╭──────────────────────────────────────────────╮");
    println!("│  Link account '{account}'");
    println!("│");
    println!("│  1. Open: {}", dc.verification_uri);
    println!("│  2. Enter code: {}", dc.user_code);
    println!("╰──────────────────────────────────────────────╯\n");
    tracing::info!(
        account,
        code = %dc.user_code,
        url = %dc.verification_uri,
        "waiting for authorisation"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(dc.expires_in_secs);
    loop {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("the device code expired before it was entered");
        }
        tokio::time::sleep(Duration::from_secs(dc.interval_secs.max(1))).await;

        match auth.poll_once(&dc).await? {
            PollOutcome::Pending => continue,
            PollOutcome::Granted {
                access_token,
                refresh_token,
            } => {
                let v = auth.validate(&access_token).await?;
                storage.put_tokens(
                    account,
                    &Tokens {
                        access_token,
                        refresh_token,
                        user_id: v.user_id.clone(),
                        login: v.login.clone(),
                        device_id,
                    },
                )?;
                println!("Linked account '{account}' to Twitch user '{}'.", v.login);
                tracing::info!(account, login = %v.login, user_id = %v.user_id, "authenticated");
                return Ok(());
            }
        }
    }
}

pub async fn status(storage: &Storage, account: &str) -> anyhow::Result<()> {
    let Some(tokens) = storage.get_tokens(account)? else {
        println!("Account '{account}' is not linked. Run: btdm login");
        return Ok(());
    };

    let http = build_http_client()?;
    let auth = AuthClient::new(http, tokens.device_id.clone());

    match auth.validate(&tokens.access_token).await {
        Ok(v) => {
            let days = v.expires_in_secs / 86_400;
            println!(
                "Account '{account}': linked to '{}' (user {}), token valid for ~{days} days.",
                v.login, v.user_id
            );
        }
        Err(e) => {
            println!("Account '{account}': stored token is NOT valid ({e}). Run: btdm login --force");
        }
    }
    Ok(())
}
