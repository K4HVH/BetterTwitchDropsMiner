//! Runs one independent miner per linked account, plus the dashboard.
//!
//! Accounts share nothing but the dashboard. A crash, a revoked token or a
//! stalled campaign on one account cannot affect another, which is exactly
//! what "independent miners, shared UI" has to mean to be worth anything.

use crate::config::Config;
use crate::run::{self, RunOptions};
use crate::storage::Storage;
use btdm_web::Hub;
use std::net::SocketAddr;
use std::sync::Arc;

/// Generates a bearer token for a dashboard bound to a non-loopback address.
fn generate_token() -> String {
    use std::hash::{BuildHasher, RandomState};
    let a = RandomState::new().hash_one("btdm-web-a");
    let b = RandomState::new().hash_one("btdm-web-b");
    format!("{a:016x}{b:016x}")
}

fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

pub struct SupervisorOptions {
    /// Do not start the dashboard.
    pub headless: bool,
    /// Mine only this account rather than every linked one.
    pub only: Option<String>,
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
        tasks.spawn(async move {
            let result = run::mine(
                &storage,
                &account,
                &config,
                RunOptions { probe: false },
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
        let token = (!is_loopback(&addr)).then(generate_token);
        if let Some(t) = &token {
            tracing::warn!(
                %addr,
                "dashboard is not on loopback; open it at http://{addr}/?token={t}"
            );
            println!("\n  Dashboard: http://{addr}/?token={t}\n");
        } else {
            println!("\n  Dashboard: http://{addr}/\n");
        }

        let hub = hub.clone();
        tasks.spawn(async move {
            let result = btdm_web::serve(hub, addr, token)
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
