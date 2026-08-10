//! Wires config, storage, auth, PubSub and the engine into a running miner.

use crate::config::Config;
use crate::storage::Storage;
use btdm_core::api::TwitchApi;
use btdm_core::engine::{Engine, EngineConfig, MiningState};
use btdm_twitch::api_impl::TwitchClient;
use btdm_twitch::auth::AuthClient;
use btdm_twitch::client::build_http_client;
use btdm_core::watchdog::{RecoveryStep, Verdict};
use btdm_twitch::pubsub::{self, PubSubMessage, Topic};
use chrono::{Duration, Utc};
use std::time::Duration as StdDuration;

/// How often campaigns are re-synced from Twitch.
const SYNC_INTERVAL: Duration = Duration::minutes(30);
/// How often the access token is re-validated.
const AUTH_CHECK_INTERVAL: Duration = Duration::minutes(60);
/// Backoff ceiling for PubSub reconnection.
const PUBSUB_MAX_BACKOFF: StdDuration = StdDuration::from_secs(180);

pub struct RunOptions {
    /// Run a single diagnostic pass and exit, rather than mining forever.
    pub probe: bool,
}

pub async fn mine(
    storage: &Storage,
    account: &str,
    config: &Config,
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

    let account_config = config.account(account);
    let engine_config = EngineConfig {
        strategy: account_config.strategy.clone(),
        favourites: account_config.favourite_channels.clone(),
        priority_games: account_config.priority_games.clone(),
        pinned: account_config.pinned_channel.clone(),
        ..EngineConfig::default()
    };
    let watch_interval = engine_config.watch_interval.to_std()?;
    let mut engine = Engine::new(api, engine_config);
    tracing::info!(
        strategy = engine.strategy_name(),
        favourites = account_config.favourite_channels.len(),
        priority_games = account_config.priority_games.len(),
        "engine configured"
    );

    // Push channel for drop progress and claim notifications. The supervisor
    // owns reconnection, so the main loop never blocks on it.
    let mut pubsub_rx =
        spawn_pubsub_supervisor(tokens.access_token.clone(), validated.user_id.clone());

    let mut last_sync = None;
    let mut last_auth_check = Utc::now();

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
            // A revoked token cannot be recovered from by retrying; say so now
            // rather than churning uselessly until the next scheduled sync.
            if matches!(e, btdm_core::api::ApiError::Unauthorized) {
                anyhow::bail!("the stored token was rejected. Run: btdm login --force");
            }
            tracing::error!(error = %e, "channel selection failed");
        }

        // --- one mining tick -------------------------------------------------
        if let Err(e) = engine.tick(now).await {
            if matches!(e, btdm_core::api::ApiError::Unauthorized) {
                anyhow::bail!("the stored token was rejected. Run: btdm login --force");
            }
            tracing::error!(error = %e, "mining tick failed; leaving this channel");
            engine.switch_away(now);
        }

        // --- watchdog ---------------------------------------------------------
        match engine.assess_health(now) {
            Verdict::Healthy | Verdict::Waiting => {}
            Verdict::Suspect => {
                tracing::debug!(
                    quiet_secs = engine.since_last_confirmation(now).map(|d| d.num_seconds()),
                    "quiet for longer than expected"
                );
            }
            Verdict::Escalate(step) => {
                if work_recovery_rung(&mut engine, &auth, &tokens.access_token, step, now).await {
                    last_sync = None;
                }
            }
            Verdict::Exhausted => {
                tracing::error!(
                    "recovery exhausted; quarantining this campaign and looking elsewhere"
                );
                engine.quarantine_current_campaign(now);
                last_sync = None;
            }
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

            tokio::select! {
                // Every branch here is cheap, so the heartbeat deadline and
                // Ctrl-C are always reachable.
                _ = tokio::time::sleep(remaining) => break,
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                    return Ok(());
                }
                msg = pubsub_rx.recv() => {
                    match msg {
                        Some(event) => {
                            handle_pubsub(&mut engine, event);
                            drain_events(&mut engine, storage, account)?;
                        }
                        // The supervisor retries forever, so this only happens
                        // if it stopped entirely.
                        None => {
                            tracing::error!("pubsub supervisor stopped; polling only");
                            tokio::time::sleep(remaining).await;
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Works one rung of the recovery ladder. Returns whether campaigns should be
/// re-synced afterwards.
///
/// Every rung logs what it did and what it found, so a recovery is a readable
/// story in the event log rather than an unexplained channel change.
async fn work_recovery_rung(
    engine: &mut Engine<TwitchClient>,
    auth: &AuthClient,
    token: &str,
    step: RecoveryStep,
    now: chrono::DateTime<Utc>,
) -> bool {
    tracing::warn!(?step, "no confirmed progress; working the recovery ladder");

    match step {
        RecoveryStep::ResendWatch => {
            engine.force_next_watch();
            false
        }
        RecoveryStep::RefreshStreamInfo => {
            match engine.refresh_stream_info(now).await {
                // The broadcast ID moved, so heartbeats were earning nothing.
                Ok(true) => engine.force_next_watch(),
                Ok(false) => tracing::debug!("stream info unchanged"),
                Err(e) => tracing::warn!(error = %e, "could not refresh stream info"),
            }
            false
        }
        RecoveryStep::ReverifyEligibility => match engine.reverify_current_channel().await {
            Ok(true) => {
                tracing::info!("Twitch still says this channel progresses our drops");
                false
            }
            Ok(false) => {
                tracing::warn!("Twitch says this channel progresses nothing; leaving");
                engine.quarantine_current(now);
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "re-verification failed");
                false
            }
        },
        RecoveryStep::SwitchChannel => {
            engine.quarantine_current(now);
            true
        }
        RecoveryStep::ResyncCampaigns => true,
        RecoveryStep::ValidateAuth => {
            match auth.validate(token).await {
                Ok(v) => tracing::info!(login = %v.login, "token is still valid"),
                Err(e) => tracing::error!(
                    error = %e,
                    "token validation failed — mining cannot recover without a new login"
                ),
            }
            false
        }
        RecoveryStep::QuarantineCampaign => {
            engine.quarantine_current_campaign(now);
            true
        }
    }
}

/// Owns the PubSub connection for the process lifetime, reconnecting forever
/// with capped backoff.
///
/// This is a separate task on purpose. Doing reconnection inline in the main
/// loop meant a slow connect blocked watch heartbeats and Ctrl-C, and a single
/// failed attempt disabled push events permanently — the retry could only be
/// triggered by a disconnect message that could no longer arrive.
fn spawn_pubsub_supervisor(
    token: String,
    user_id: String,
) -> tokio::sync::mpsc::Receiver<PubSubMessage> {
    let (tx, rx) = tokio::sync::mpsc::channel(128);

    tokio::spawn(async move {
        let topics = vec![
            Topic::UserDrops.as_str(&user_id),
            Topic::UserPoints.as_str(&user_id),
        ];
        let mut backoff = StdDuration::from_secs(1);

        loop {
            match pubsub::connect(pubsub::PUBSUB_URL, &token, topics.clone()).await {
                Ok(mut socket) => {
                    tracing::info!("pubsub connected");
                    backoff = StdDuration::from_secs(1);

                    while let Some(msg) = socket.recv().await {
                        if matches!(msg, PubSubMessage::Disconnected) {
                            break;
                        }
                        // The consumer going away means the miner is shutting
                        // down; stop rather than reconnecting into the void.
                        if tx.send(msg).await.is_err() {
                            return;
                        }
                    }
                    tracing::warn!(
                        "pubsub disconnected; the poll backstop is the only progress \
                         source until it is back"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        retry_in_secs = backoff.as_secs(),
                        "pubsub connect failed; will retry"
                    );
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(PUBSUB_MAX_BACKOFF);
        }
    });

    rx
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
