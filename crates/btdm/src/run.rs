//! Wires config, storage, auth, PubSub and the engine into a running miner.

use crate::config::Config;
use crate::storage::Storage;
use btdm_core::api::TwitchApi;
use btdm_core::engine::{Engine, EngineConfig, MiningState};
use btdm_twitch::api_impl::TwitchClient;
use btdm_twitch::auth::AuthClient;
use btdm_twitch::client::build_http_client;
use btdm_twitch::pubsub::{self, PubSubMessage, Topic};
use chrono::{Duration, Utc};
use std::time::Duration as StdDuration;

/// How often campaigns are re-synced from Twitch.
const SYNC_INTERVAL: Duration = Duration::minutes(30);
/// How often the access token is re-validated.
const AUTH_CHECK_INTERVAL: Duration = Duration::minutes(60);
/// No confirmed progress for this long means the session is not mining.
const STALL_THRESHOLD: Duration = Duration::seconds(360);
/// Backoff ceiling for PubSub reconnection.
const PUBSUB_MAX_BACKOFF: StdDuration = StdDuration::from_secs(180);

pub struct RunOptions {
    /// Run a single diagnostic pass and exit, rather than mining forever.
    pub probe: bool,
}

pub async fn mine(
    storage: &Storage,
    account: &str,
    _config: &Config,
    opts: RunOptions,
) -> anyhow::Result<()> {
    let tokens = storage
        .get_tokens(account)?
        .ok_or_else(|| anyhow::anyhow!("account '{account}' is not linked. Run: btdm login"))?;

    let http = build_http_client()?;
    let auth = AuthClient::new(http.clone(), tokens.device_id.clone());
    let validated = auth.validate(&tokens.access_token).await?;
    tracing::info!(login = %validated.login, user_id = %validated.user_id, "authenticated");

    let api = TwitchClient::new(
        http,
        tokens.access_token.clone(),
        btdm_core::model::UserId(validated.user_id.clone()),
        validated.login.clone(),
    );

    let engine_config = EngineConfig::default();
    let watch_interval = engine_config.watch_interval.to_std()?;
    let mut engine = Engine::new(api, engine_config);

    // Push channel for drop progress and claim notifications.
    let mut pubsub_rx = connect_pubsub(&tokens.access_token, &validated.user_id).await;

    let mut last_sync = None;
    let mut last_auth_check = Utc::now();
    let mut pubsub_backoff = StdDuration::from_secs(1);

    loop {
        let now = Utc::now();

        // --- campaign sync -------------------------------------------------
        if last_sync.is_none_or(|t| now - t >= SYNC_INTERVAL) {
            match engine.sync(now).await {
                Ok(()) => last_sync = Some(now),
                Err(btdm_core::api::ApiError::Unauthorized) => {
                    anyhow::bail!("the stored token was rejected. Run: btdm login --force");
                }
                Err(e) => tracing::error!(error = %e, "campaign sync failed"),
            }
        }

        // --- channel selection ---------------------------------------------
        if engine.current_channel().is_none()
            && let Err(e) = engine.select_channel(now).await
        {
            tracing::error!(error = %e, "channel selection failed");
        }

        // --- one mining tick -------------------------------------------------
        if let Err(e) = engine.tick(now).await {
            tracing::error!(error = %e, "mining tick failed; leaving this channel");
            engine.switch_away(now);
        }

        // --- watchdog ---------------------------------------------------------
        if engine.current_channel().is_some()
            && let Some(since) = engine.since_last_confirmation(now)
            && since >= STALL_THRESHOLD
        {
            tracing::warn!(
                stalled_for_secs = since.num_seconds(),
                state = ?engine.state(),
                "no confirmed progress; abandoning this channel"
            );
            engine.switch_away(now);
            // Force a fresh look at the world before choosing again.
            last_sync = None;
        }

        // --- token liveness ----------------------------------------------------
        if now - last_auth_check >= AUTH_CHECK_INTERVAL {
            last_auth_check = now;
            match auth.validate(&tokens.access_token).await {
                Ok(_) => tracing::debug!("token still valid"),
                Err(e) => tracing::error!(error = %e, "token validation failed"),
            }
        }

        drain_events(&mut engine, storage, account)?;
        report_status(&engine, now);

        if opts.probe {
            tracing::info!("probe mode: one pass complete");
            return Ok(());
        }

        // --- wait, servicing pushed events while we do ------------------------
        let deadline = tokio::time::Instant::now() + watch_interval;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            let recv = async {
                match pubsub_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            };

            tokio::select! {
                _ = tokio::time::sleep(remaining) => break,
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                    return Ok(());
                }
                msg = recv => {
                    match msg {
                        Some(PubSubMessage::Disconnected) | None => {
                            pubsub_rx = None;
                            tracing::warn!(
                                backoff_secs = pubsub_backoff.as_secs(),
                                "pubsub disconnected; the poll backstop is now the only \
                                 progress source"
                            );
                            tokio::time::sleep(pubsub_backoff).await;
                            pubsub_backoff = (pubsub_backoff * 2).min(PUBSUB_MAX_BACKOFF);
                            pubsub_rx =
                                connect_pubsub(&tokens.access_token, &validated.user_id).await;
                            if pubsub_rx.is_some() {
                                pubsub_backoff = StdDuration::from_secs(1);
                            }
                        }
                        Some(other) => {
                            handle_pubsub(&mut engine, other);
                            drain_events(&mut engine, storage, account)?;
                        }
                    }
                }
            }
        }
    }
}

async fn connect_pubsub(
    token: &str,
    user_id: &str,
) -> Option<tokio::sync::mpsc::Receiver<PubSubMessage>> {
    let topics = vec![
        Topic::UserDrops.as_str(user_id),
        Topic::UserPoints.as_str(user_id),
    ];
    match pubsub::connect(pubsub::PUBSUB_URL, token, topics).await {
        Ok(rx) => {
            tracing::info!("pubsub connected");
            Some(rx)
        }
        Err(e) => {
            tracing::warn!(error = %e, "pubsub connect failed; relying on the poll backstop");
            None
        }
    }
}

fn handle_pubsub<A: TwitchApi>(engine: &mut Engine<A>, msg: PubSubMessage) {
    let now = Utc::now();
    match msg {
        PubSubMessage::DropProgress(p) => engine.on_pushed_progress(p, now),
        PubSubMessage::DropClaimable { drop_id, claim_id } => {
            tracing::info!(drop = %drop_id, "drop is claimable");
            engine.on_drop_claimable(&drop_id, &claim_id);
        }
        PubSubMessage::StreamDown { channel_id } => {
            if engine.current_channel().is_some_and(|c| c.id == channel_id) {
                tracing::info!(channel = %channel_id, "watched channel went offline");
                engine.switch_away(now);
            }
        }
        PubSubMessage::StreamUp { .. }
        | PubSubMessage::PointsClaimable { .. }
        | PubSubMessage::Disconnected => {}
    }
}

fn drain_events<A: TwitchApi>(
    engine: &mut Engine<A>,
    storage: &Storage,
    account: &str,
) -> anyhow::Result<()> {
    for event in engine.drain_events() {
        tracing::info!(kind = ?event.kind, "event");
        storage.append_event(account, &event)?;
    }
    Ok(())
}

/// One honest line about what is actually happening. Nothing here is inferred
/// from a local timer — if Twitch has not confirmed progress, this says so.
fn report_status<A: TwitchApi>(engine: &Engine<A>, now: chrono::DateTime<Utc>) {
    let channel = engine
        .current_channel()
        .map(|c| c.login.clone())
        .unwrap_or_else(|| "-".into());
    let since = engine
        .since_last_confirmation(now)
        .map(|d| format!("{}s ago", d.num_seconds()))
        .unwrap_or_else(|| "never".into());

    // Only the drop that actually advanced may be shown. Picking any other
    // drop with non-zero minutes reports numbers that belong to something the
    // miner is not currently earning.
    let progress = engine
        .active_drop()
        .map(|(_, p)| format!("{}/{} min", p.current_minutes, p.required_minutes))
        .unwrap_or_else(|| "no confirmed progress".into());

    match engine.state() {
        MiningState::Idle => tracing::info!("idle — no eligible channel"),
        MiningState::Establishing => tracing::info!(
            channel,
            last_confirmed = since,
            "watching, awaiting first confirmation"
        ),
        MiningState::Mining => {
            tracing::info!(channel, confirmed = progress, last_confirmed = since, "mining")
        }
    }
}
