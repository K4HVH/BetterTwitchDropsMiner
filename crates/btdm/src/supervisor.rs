//! Runs one independent miner per linked account, plus the dashboard.
//!
//! Accounts share nothing but the dashboard. A crash, a revoked token or a
//! stalled campaign on one account cannot affect another, which is exactly
//! what "independent miners, shared UI" has to mean to be worth anything.

use crate::config::{Config, ConfigStore};
use crate::run::{self, RunOptions};
use crate::storage::Storage;
use btdm_web::Hub;
use std::net::SocketAddr;
use std::sync::Arc;

/// Generates a bearer token for a dashboard bound to a non-loopback address.
///
/// This guards a page that reflects a live Twitch session on whatever network
/// the miner is reachable from, so it comes from the OS rather than from a
/// hasher seeded for collision resistance.
fn generate_token() -> String {
    btdm_twitch::client::random_hex_128()
}

fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

pub struct SupervisorOptions {
    /// Do not start the dashboard.
    pub headless: bool,
    /// Mine only this account rather than every linked one.
    pub only: Option<String>,
    /// Where settings changed from the dashboard are written back.
    pub config_store: ConfigStore,
}

pub async fn run(
    storage: Arc<Storage>,
    config: Config,
    opts: SupervisorOptions,
) -> anyhow::Result<()> {
    let linked = storage.linked_accounts()?;
    let accounts: Vec<String> = match &opts.only {
        Some(name) => {
            if !linked.iter().any(|a| a == name) {
                anyhow::bail!("account '{name}' is not linked. Run: btdm login --account {name}");
            }
            vec![name.clone()]
        }
        None => linked,
    };

    if accounts.is_empty() {
        anyhow::bail!("no accounts are linked. Run: btdm login");
    }
    tracing::info!(accounts = ?accounts, "starting miners");

    let hub = Hub::new();
    let mut tasks = tokio::task::JoinSet::new();

    for account in accounts {
        let storage = Arc::clone(&storage);
        let config = config.clone();
        let hub = hub.clone();
        let config_store = opts.config_store.clone();
        tasks.spawn(async move {
            let result = run::mine(
                &storage,
                &account,
                &config,
                RunOptions {
                    probe: false,
                    config_store: Some(config_store),
                },
                Some(hub),
            )
            .await;
            (account, result)
        });
    }

    if !opts.headless {
        let addr: SocketAddr = config
            .bind
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid bind address '{}': {e}", config.bind))?;

        // A dashboard reachable from the network exposes state derived from a
        // live Twitch session, so it is never left open.
        //
        // The token is kept in the config once generated. Minting a fresh one
        // on every start invalidated whatever URL the user had bookmarked and
        // left `btdm control` unable to authenticate at all.
        let token = if is_loopback(&addr) {
            None
        } else {
            let existing = config.dashboard_token.clone();
            let token = match existing {
                Some(t) if !t.is_empty() => t,
                _ => {
                    let fresh = generate_token();
                    if let Err(e) = opts.config_store.save_dashboard_token(&fresh).await {
                        tracing::warn!(
                            error = %e,
                            "could not save the dashboard token; it will change on restart"
                        );
                    }
                    fresh
                }
            };
            tracing::warn!(%addr, "dashboard is not on loopback; a token is required");
            Some(token)
        };

        let hub = hub.clone();
        tasks.spawn(async move {
            // The URL is announced from inside, once the socket is bound, so a
            // failed bind never leaves a working-looking URL on screen.
            let result = btdm_web::serve_announcing(hub, addr, token, |url| {
                println!("\n  Dashboard: {url}\n");
            })
            .await
            .map_err(anyhow::Error::from);
            ("dashboard".to_string(), result)
        });
    }

    // The first task to finish ends the process: a miner only returns on a
    // fatal, unrecoverable error, and the dashboard only on a bind failure.
    // Reporting which one, and why, beats exiting silently.
    if let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((who, Err(e))) => {
                tracing::error!(task = %who, error = %e, "task failed");
                return Err(e.context(format!("{who} failed")));
            }
            Ok((who, Ok(()))) => tracing::info!(task = %who, "task finished"),
            Err(e) => tracing::error!(error = %e, "task panicked"),
        }
    }
    tasks.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_32_hex_chars_and_differ() {
        let a = generate_token();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, generate_token());
    }

    #[test]
    fn loopback_is_recognised() {
        assert!(is_loopback(&"127.0.0.1:8080".parse().unwrap()));
        assert!(is_loopback(&"[::1]:8080".parse().unwrap()));
    }

    /// Binding to anything reachable from the network must trigger the token,
    /// because the dashboard reflects a live Twitch session.
    #[test]
    fn non_loopback_addresses_are_not_treated_as_local() {
        assert!(!is_loopback(&"0.0.0.0:8080".parse().unwrap()));
        assert!(!is_loopback(&"192.168.1.10:8080".parse().unwrap()));
    }
}
