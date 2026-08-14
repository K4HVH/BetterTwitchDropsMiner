//! `btdm control` — drive a running miner from the command line.
//!
//! The dashboard is the richer interface, but a headless box is a stated
//! target for this project and "install a browser, or edit TOML and restart"
//! is not an acceptable answer there. Every control on the page is reachable
//! here, over the same HTTP API and the same authorisation.

use crate::config::Config;
use btdm_core::control::Command;
use btdm_core::snapshot::Snapshot;

/// Where the running miner's API is, and how to authenticate to it.
#[derive(Debug)]
pub struct Endpoint {
    base: String,
    token: Option<String>,
}

impl Endpoint {
    /// Derives the endpoint from the config the daemon is using.
    ///
    /// `0.0.0.0` is a bind address, not a destination: connecting to it works
    /// on Linux but is meaningless, so it is rewritten to loopback for the
    /// common "control the miner on this machine" case.
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let bind = config.bind.trim();
        let (host, port) = bind
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("invalid bind address '{bind}': no port"))?;

        let host = match host.trim_matches(['[', ']']) {
            "0.0.0.0" | "" => "127.0.0.1",
            "::" => "[::1]",
            other if other.contains(':') => return Ok(Self::ipv6(other, port, config)),
            other => other,
        };

        Ok(Self {
            base: format!("http://{host}:{port}"),
            token: config.dashboard_token.clone(),
        })
    }

    fn ipv6(host: &str, port: &str, config: &Config) -> Self {
        Self {
            base: format!("http://[{host}]:{port}"),
            token: config.dashboard_token.clone(),
        }
    }

    fn url(&self, path: &str) -> String {
        match &self.token {
            Some(t) => format!("{}{path}?token={t}", self.base),
            None => format!("{}{path}", self.base),
        }
    }
}

/// Turns a transport failure into advice, because "connection refused" on its
/// own does not tell anyone what to do about it.
fn unreachable(base: &str, e: reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "could not reach the miner at {base} ({e}).\n\
         Is it running? Start it with `btdm mine`. If the dashboard is bound \
         elsewhere, check `bind` in config.toml."
    )
}

pub async fn send(config: &Config, account: &str, command: Command) -> anyhow::Result<()> {
    let endpoint = Endpoint::from_config(config)?;
    let http = reqwest::Client::new();

    let res = http
        .post(endpoint.url(&format!("/api/accounts/{account}/command")))
        .json(&command)
        .send()
        .await
        .map_err(|e| unreachable(&endpoint.base, e))?;

    let status = res.status();
    let body: serde_json::Value = res.json().await.unwrap_or_default();

    // 202 means the miner took the command and had not acted on it before we
    // stopped waiting. That is not a confirmation, and printing it like one
    // would tell a script the setting had been applied when it may never be:
    // a miner that exits before draining its queue drops the command.
    if status == reqwest::StatusCode::ACCEPTED {
        anyhow::bail!(
            "the miner accepted the command but had not applied it after {}s, so \
             the outcome is unknown. Check `btdm status`; if the miner is wedged \
             the command may never run.",
            btdm_web::COMMAND_TIMEOUT.as_secs()
        );
    }
    if status.is_success() {
        // The miner's own words. "Sent." was all this could say before the
        // reply carried anything, which meant the CLI could not tell a command
        // that worked from one the miner declined as a no-op.
        match body.get("message").and_then(|m| m.as_str()) {
            Some(m) if !m.is_empty() => println!("{m}"),
            _ => println!("Sent."),
        }
        return Ok(());
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!(
            "the running miner has no account called '{account}'. \
             Use --account, or check `btdm status`."
        );
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!(
            "the miner rejected the request: no valid token. The dashboard is \
             bound off loopback, so `dashboard_token` in config.toml must match \
             the running daemon's."
        );
    }
    anyhow::bail!(
        "the miner refused the command ({status}): {}",
        body.get("error").and_then(|e| e.as_str()).unwrap_or("no reason given")
    )
}

/// How `btdm control priority` is changing the list.
#[derive(Debug)]
pub enum PriorityChange {
    /// Replace it outright.
    Set(Vec<String>),
    Add(String),
    Remove(String),
    /// Reorder by one place. Relative, so it must never be retried blindly.
    Move { entry: String, up: bool },
    Clear,
}

/// Applies a change to the priority list, then prints the result.
///
/// Single-entry edits go to the miner as single-entry commands rather than as
/// a rewritten list. Assembling the new list here would mean reading the
/// published snapshot first, which lags whatever the miner has already
/// applied — two quick `--add` calls both started from the same stale list and
/// the second silently discarded the first.
pub async fn set_priority(
    config: &Config,
    account: &str,
    change: PriorityChange,
) -> anyhow::Result<()> {
    let command = match change {
        PriorityChange::Set(games) => Command::SetPriorityGames { games },
        PriorityChange::Clear => Command::SetPriorityGames { games: Vec::new() },
        PriorityChange::Add(entry) => Command::AddPriorityEntry { entry },
        PriorityChange::Remove(entry) => Command::RemovePriorityEntry { entry },
        PriorityChange::Move { entry, up } => Command::MovePriorityEntry { entry, up },
    };
    // Prints the miner's confirmation, which is synchronous with the change.
    // Reading the list back from `/api/snapshots` instead would race the
    // miner's publish schedule and print the list as it was *before* the edit.
    send(config, account, command).await
}

/// Prints what every running account is doing. The point is that this is the
/// same data the dashboard renders, so the two can never disagree.
///
/// `json` emits the snapshot verbatim, for a monitoring script or a status
/// bar — the human format is for reading, not for parsing.
pub async fn show(config: &Config, json: bool) -> anyhow::Result<()> {
    let endpoint = Endpoint::from_config(config)?;
    let http = reqwest::Client::new();

    let res = http
        .get(endpoint.url("/api/snapshots"))
        .send()
        .await
        .map_err(|e| unreachable(&endpoint.base, e))?;

    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("the miner rejected the request: no valid token in config.toml");
    }
    if json {
        // Straight through, so a script sees exactly what the dashboard does.
        println!("{}", res.text().await?);
        return Ok(());
    }

    let snapshots: Vec<Snapshot> = res.json().await?;

    if snapshots.is_empty() {
        println!("The miner is running but no account has reported yet.");
        return Ok(());
    }

    for s in snapshots {
        println!("─── {} {}", s.account, s.login.as_deref().unwrap_or(""));
        println!("    strategy   {}  ({})", s.strategy, s.state.to_lowercase());
        match &s.watching {
            Some(w) => {
                println!(
                    "    watching   {} — {}",
                    w.channel_display_name,
                    w.campaigns.join(", ")
                );
                match (w.current_minutes, w.required_minutes) {
                    (Some(c), Some(r)) => println!(
                        "    confirmed  {c}/{r} min{}",
                        w.drop_name
                            .as_deref()
                            .map(|n| format!("  ({n})"))
                            .unwrap_or_default()
                    ),
                    // Never invent a number. "Not yet confirmed" is the honest
                    // answer and the whole point of this project.
                    _ => println!("    confirmed  nothing yet"),
                }
                if let Some(secs) = w.seconds_since_confirmation {
                    println!(
                        "    last seen  {secs}s ago{}",
                        if w.stalled { "   STALLED" } else { "" }
                    );
                }
                println!("    why        {}", w.reason);
            }
            None if s.paused => println!("    watching   nothing (paused)"),
            None => println!("    watching   nothing"),
        }
        println!(
            "    campaigns  {} of {} mineable, {} at risk",
            s.totals.campaigns_eligible, s.totals.campaigns_known, s.totals.campaigns_at_risk
        );
        println!(
            "    drops      {} claimed, {} remaining ({} min of watching left)",
            s.totals.drops_claimed, s.totals.drops_remaining, s.totals.minutes_remaining
        );
        if let Some(step) = &s.health.recovery_step {
            println!("    recovering {step}");
        }
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(bind: &str, token: Option<&str>) -> Config {
        Config {
            bind: bind.into(),
            dashboard_token: token.map(str::to_owned),
            ..Config::default()
        }
    }

    #[test]
    fn a_loopback_bind_is_used_as_is() {
        let e = Endpoint::from_config(&config("127.0.0.1:8080", None)).unwrap();
        assert_eq!(e.url("/api/health"), "http://127.0.0.1:8080/api/health");
    }

    /// `0.0.0.0` means "listen everywhere"; connecting to it is not what the
    /// user meant when they said "control the miner on this machine".
    #[test]
    fn a_wildcard_bind_is_rewritten_to_loopback() {
        let e = Endpoint::from_config(&config("0.0.0.0:9000", None)).unwrap();
        assert_eq!(e.url("/api/health"), "http://127.0.0.1:9000/api/health");
    }

    #[test]
    fn the_ipv6_wildcard_is_rewritten_too() {
        let e = Endpoint::from_config(&config("[::]:9000", None)).unwrap();
        assert!(e.url("/x").starts_with("http://[::1]:9000"), "{}", e.url("/x"));
    }

    #[test]
    fn a_specific_address_is_kept() {
        let e = Endpoint::from_config(&config("192.168.1.50:8080", None)).unwrap();
        assert_eq!(e.url("/api/health"), "http://192.168.1.50:8080/api/health");
    }

    #[test]
    fn the_token_is_attached_when_one_is_configured() {
        let e = Endpoint::from_config(&config("0.0.0.0:8080", Some("secret"))).unwrap();
        assert_eq!(e.url("/api/health"), "http://127.0.0.1:8080/api/health?token=secret");
    }

    #[test]
    fn a_bind_without_a_port_is_a_clear_error() {
        let err = Endpoint::from_config(&config("localhost", None)).unwrap_err();
        assert!(err.to_string().contains("no port"), "{err}");
    }
}
