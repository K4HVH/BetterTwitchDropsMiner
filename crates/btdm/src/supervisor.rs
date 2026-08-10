//! Runs one independent miner per linked account, plus the dashboard.
//!
//! Accounts share nothing but the dashboard. A crash, a revoked token or a
//! stalled campaign on one account cannot affect another, which is exactly
//! what "independent miners, shared UI" has to mean to be worth anything.
//!
//! That principle used to stop at the supervisor: the first task to finish
//! ended the process, so a single expired token took down every other
//! account's miner *and* the dashboard — the one thing that could have told
//! you what happened, or let you fix it.

use crate::config::{Config, ConfigStore};
use crate::run::{self, RunOptions};
use crate::storage::Storage;
use btdm_web::Hub;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

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

    // With a dashboard there is somewhere to log in, so no linked account is a
    // starting state rather than a fatal one. Headless, it is fatal: nothing
    // would ever happen and nobody would be told why.
    if accounts.is_empty() && opts.headless {
        anyhow::bail!("no accounts are linked. Run: btdm login");
    }
    tracing::info!(accounts = ?accounts, "starting miners");

    let hub = Hub::new();
    // The dashboard reads the stored log through this, so a page opened after
    // a restart shows what happened while nobody was looking.
    hub.set_history(Arc::new(StoredHistory(Arc::clone(&storage))));

    let (start_tx, mut start_rx) = mpsc::channel::<String>(8);
    hub.set_enrolment(Arc::new(crate::enrol::Enroller::new(
        Arc::clone(&storage),
        hub.clone(),
        start_tx.clone(),
    )));

    let mut miners = tokio::task::JoinSet::new();
    let spawn_miner = |miners: &mut tokio::task::JoinSet<_>, account: String| {
        let storage = Arc::clone(&storage);
        let config = config.clone();
        let hub = hub.clone();
        let config_store = opts.config_store.clone();
        miners.spawn(async move {
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
    };

    for account in accounts {
        spawn_miner(&mut miners, account);
    }

    // The dashboard is kept out of the miner set on purpose: it is the thing
    // that survives them.
    let mut dashboard = if opts.headless {
        None
    } else {
        Some(spawn_dashboard(&config, &opts, hub.clone()).await?)
    };

    loop {
        tokio::select! {
            joined = miners.join_next(), if !miners.is_empty() => {
                let Some(joined) = joined else { continue };
                match joined {
                    Ok((who, Err(e))) => {
                        tracing::error!(account = %who, error = %e, "miner stopped");
                        hub.set_account_error(&who, &e.to_string());
                        if dashboard.is_none() && miners.is_empty() {
                            // Headless, with nothing left running and nowhere
                            // to fix it. Exiting with the reason beats sitting
                            // there doing nothing.
                            return Err(e.context(format!("{who} failed")));
                        }
                        tracing::warn!(
                            account = %who,
                            "other accounts keep mining; re-authorise from the dashboard \
                             or run: btdm login --force --account {who}"
                        );
                    }
                    Ok((who, Ok(()))) => {
                        tracing::info!(account = %who, "miner finished");
                        // A clean finish is Ctrl-C, which means all of them.
                        break;
                    }
                    Err(e) => tracing::error!(error = %e, "miner panicked"),
                }
            }
            Some(account) = start_rx.recv() => {
                tracing::info!(%account, "starting miner after login");
                hub.clear_account_error(&account);
                spawn_miner(&mut miners, account);
            }
            result = async { dashboard.as_mut().unwrap().await }, if dashboard.is_some() => {
                // The dashboard only ends on a bind failure, which is fatal:
                // without it there is no way to see or fix anything.
                return match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e.context("the dashboard stopped")),
                    Err(e) => Err(anyhow::anyhow!("the dashboard panicked: {e}")),
                };
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                break;
            }
        }
    }

    miners.shutdown().await;
    Ok(())
}

async fn spawn_dashboard(
    config: &Config,
    opts: &SupervisorOptions,
    hub: Hub,
) -> anyhow::Result<tokio::task::JoinHandle<anyhow::Result<()>>> {
    let addr: SocketAddr = config
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address '{}': {e}", config.bind))?;

    // A dashboard reachable from the network exposes state derived from a
    // live Twitch session, so it is never left open.
    //
    // The token is kept in the config once generated. Minting a fresh one on
    // every start invalidated whatever URL the user had bookmarked and left
    // `btdm control` unable to authenticate at all.
    let token = if is_loopback(&addr) {
        None
    } else {
        let token = match config.dashboard_token.clone() {
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

    Ok(tokio::spawn(async move {
        // The URL is announced from inside, once the socket is bound, so a
        // failed bind never leaves a working-looking URL on screen.
        btdm_web::serve_announcing(hub, addr, token, |url| {
            println!("\n  Dashboard: {url}\n");
        })
        .await
        .map_err(anyhow::Error::from)
    }))
}

/// Adapts the miner's SQLite log to what the dashboard reads.
struct StoredHistory(Arc<Storage>);

impl btdm_web::History for StoredHistory {
    fn recent(&self, account: &str, limit: u32) -> Vec<btdm_web::HistoryRow> {
        match self.0.recent_events(account, limit) {
            Ok(rows) => rows
                .into_iter()
                .map(|r| btdm_web::HistoryRow {
                    at: r.at,
                    summary: r.summary,
                    severity: r.severity,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read the stored event log");
                Vec::new()
            }
        }
    }

    fn rewards(&self, account: &str, limit: u32) -> Vec<btdm_web::RewardRow> {
        match self.0.claimed_drops(account, limit) {
            Ok(rows) => rows
                .into_iter()
                .map(|r| btdm_web::RewardRow {
                    at: r.at,
                    name: r.name,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read claimed drops");
                Vec::new()
            }
        }
    }
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
