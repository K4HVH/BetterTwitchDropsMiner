//! Wires config, storage, auth, PubSub and the engine into a running miner.

use crate::config::{Config, ConfigStore};
use crate::notify::Discord;
use crate::storage::Storage;
use btdm_core::api::TwitchApi;
use btdm_core::control::Command;
use btdm_core::engine::{Engine, EngineConfig, MiningState};
use btdm_core::model::ChannelId;
use btdm_core::watchdog::{RecoveryStep, Verdict};
use btdm_twitch::api_impl::TwitchClient;
use btdm_twitch::auth::AuthClient;
use btdm_twitch::client::{build_http_client, build_http_client_with_proxy};
use btdm_twitch::pubsub::{self, PubSubMessage, Topic};
use btdm_web::Hub;
use chrono::{Duration, Utc};
use std::time::Duration as StdDuration;
use tokio::sync::mpsc;

/// How often the access token is re-validated.
const AUTH_CHECK_INTERVAL: Duration = Duration::minutes(60);
/// Backoff ceiling for PubSub reconnection.
const PUBSUB_MAX_BACKOFF: StdDuration = StdDuration::from_secs(180);
/// How many queued commands before the dashboard is told to slow down.
const COMMAND_QUEUE: usize = 32;

pub struct RunOptions {
    /// Run a single diagnostic pass and exit, rather than mining forever.
    pub probe: bool,
    /// Where to write settings changed from the dashboard.
    pub config_store: Option<ConfigStore>,
}

pub async fn mine(
    storage: &Storage,
    account: &str,
    config: &Config,
    opts: RunOptions,
    hub: Option<Hub>,
) -> anyhow::Result<()> {
    let tokens = storage
        .get_tokens(account)?
        .ok_or_else(|| anyhow::anyhow!("account '{account}' is not linked. Run: btdm login"))?;

    // A per-account proxy overrides the global one; this is the mitigation for
    // several accounts mining from a single address.
    let proxy = account_config_proxy(config, account);
    let http = build_http_client_with_proxy(proxy.as_deref())
        .map_err(|e| anyhow::anyhow!("could not build the HTTP client (proxy {proxy:?}): {e}"))?;
    if let Some(p) = &proxy {
        tracing::info!(proxy = %p, account, "routing Twitch traffic through a proxy");
    }
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
    let mut engine_config = EngineConfig {
        strategy: account_config.strategy.clone(),
        favourites: account_config.favourite_channels.clone(),
        priority_games: account_config.priority_games.clone(),
        pinned: account_config.pinned_channel.clone(),
        excluded_campaigns: account_config.excluded_campaigns.clone(),
        excluded_games: account_config.excluded_games.clone(),
        paused: account_config.paused,
        ..EngineConfig::default()
    };
    config.timing.apply(&mut engine_config);
    let watch_interval = engine_config.watch_interval.to_std()?;
    let mut engine = Engine::new(api, engine_config);
    tracing::info!(
        strategy = engine.strategy_name(),
        favourites = account_config.favourite_channels.len(),
        priority_games = account_config.priority_games.len(),
        paused = engine.is_paused(),
        "engine configured"
    );

    // Push channel for drop progress and claim notifications. The supervisor
    // owns reconnection, so the main loop never blocks on it.
    let (mut pubsub_rx, pubsub_follow) =
        spawn_pubsub_supervisor(tokens.access_token.clone(), validated.user_id.clone());
    // The channel currently subscribed for stream-state, so we only ask for a
    // change when there has actually been one.
    let mut followed: Option<ChannelId> = None;

    // Commands from the dashboard. Registered before the first tick so a
    // control pressed during startup is queued rather than rejected.
    let (command_tx, mut command_rx) = mpsc::channel::<Command>(COMMAND_QUEUE);
    if let Some(hub) = &hub {
        hub.register(account, command_tx);
    }

    let notifier = config
        .discord_webhook
        .as_ref()
        .map(|w| Discord::new(build_http_client().unwrap_or_default(), w.clone()));

    let mut last_auth_check = Utc::now();
    let result = loop {
        let now = Utc::now();

        // --- campaign sync -------------------------------------------------
        if engine.needs_sync(now) {
            match engine.sync(now).await {
                Ok(()) => {}
                Err(btdm_core::api::ApiError::Unauthorized) => {
                    break Err(anyhow::anyhow!(
                        "the stored token was rejected. Run: btdm login --force"
                    ));
                }
                Err(e) => tracing::error!(error = %e, "campaign sync failed"),
            }
        }

        // --- channel selection ---------------------------------------------
        if engine.needs_selection(now)
            && let Err(e) = engine.select_channel(now).await
        {
            // A revoked token cannot be recovered from by retrying; say so now
            // rather than churning uselessly until the next scheduled sync.
            if matches!(e, btdm_core::api::ApiError::Unauthorized) {
                break Err(anyhow::anyhow!(
                    "the stored token was rejected. Run: btdm login --force"
                ));
            }
            tracing::error!(error = %e, "channel selection failed");
        }

        // --- one mining tick -------------------------------------------------
        if let Err(e) = engine.tick(now).await {
            if matches!(e, btdm_core::api::ApiError::Unauthorized) {
                break Err(anyhow::anyhow!(
                    "the stored token was rejected. Run: btdm login --force"
                ));
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
                work_recovery_rung(&mut engine, &auth, &tokens.access_token, step, now).await;
            }
            Verdict::Exhausted => {
                tracing::error!(
                    "recovery exhausted; quarantining this campaign and looking elsewhere"
                );
                engine.quarantine_current_campaign(now);
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

        drain_events(&mut engine, storage, account, hub.as_ref(), notifier.as_ref()).await?;
        report_status(&engine, now);
        publish(&engine, account, &validated.login, hub.as_ref());

        let watching = engine.current_channel().map(|c| c.id.clone());
        if watching != followed {
            let _ = pubsub_follow.send(watching.clone()).await;
            followed = watching;
        }

        if opts.probe {
            tracing::info!("probe mode: one pass complete");
            break Ok(());
        }

        // --- wait, servicing pushed events and commands while we do -----------
        let deadline = tokio::time::Instant::now() + watch_interval;
        let mut shutdown = false;
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
                    shutdown = true;
                    break;
                }
                cmd = command_rx.recv() => {
                    let Some(cmd) = cmd else {
                        // Only happens if the hub dropped the sender.
                        tracing::debug!("command channel closed");
                        tokio::time::sleep(remaining).await;
                        break;
                    };
                    let effect = engine.apply_command(cmd, Utc::now());
                    tracing::info!(message = %effect.message, "command applied");

                    if effect.persist && let Some(store) = &opts.config_store
                        && let Err(e) = store.save_account(account, &engine.settings()).await {
                        tracing::error!(error = %e, "could not save settings");
                    }

                    drain_events(
                        &mut engine, storage, account, hub.as_ref(), notifier.as_ref(),
                    ).await?;
                    publish(&engine, account, &validated.login, hub.as_ref());

                    // Re-run the main loop at once so the effect is visible
                    // immediately rather than at the end of the heartbeat wait.
                    // Breaking here is the whole point of the control plane:
                    // otherwise a strategy change waits out the heartbeat.
                    if effect.reselect || effect.resync {
                        break;
                    }
                }
                msg = pubsub_rx.recv() => {
                    match msg {
                        Some(event) => {
                            handle_pubsub(&mut engine, event).await;
                            drain_events(
                                &mut engine,
                                storage,
                                account,
                                hub.as_ref(),
                                notifier.as_ref(),
                            )
                            .await?;
                            publish(&engine, account, &validated.login, hub.as_ref());
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

        if shutdown {
            break Ok(());
        }
    };

    if let Some(hub) = &hub {
        hub.unregister(account);
    }
    result
}

/// The proxy this account should use, if any.
fn account_config_proxy(config: &Config, account: &str) -> Option<String> {
    config
        .account(account)
        .proxy
        .or_else(|| config.proxy.clone())
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
}

/// Publishes a fresh snapshot. Called after anything that can change state, so
/// the dashboard is never more than one action behind.
fn publish<A: TwitchApi>(engine: &Engine<A>, account: &str, login: &str, hub: Option<&Hub>) {
    if let Some(hub) = hub {
        hub.publish(engine.snapshot(account, Some(login), Utc::now()));
    }
}

/// Works one rung of the ladder.
///
/// The mechanics live in the engine so they are testable against a fake; only
/// the auth rung is handled here, because the engine holds no credentials.
async fn work_recovery_rung(
    engine: &mut Engine<TwitchClient>,
    auth: &AuthClient,
    token: &str,
    step: RecoveryStep,
    now: chrono::DateTime<Utc>,
) {
    let outcome = match engine.apply_recovery(step, now).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, ?step, "recovery rung failed");
            return;
        }
    };

    if outcome.validate_auth {
        match auth.validate(token).await {
            Ok(v) => tracing::info!(login = %v.login, "token is still valid"),
            Err(e) => tracing::error!(
                error = %e,
                "token validation failed — mining cannot recover without a new login"
            ),
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
) -> (mpsc::Receiver<PubSubMessage>, mpsc::Sender<Option<ChannelId>>) {
    let (tx, rx) = mpsc::channel(128);
    // Which channel's stream state to follow. Sent by the main loop whenever
    // the watched channel changes, so we hear the moment a stream ends rather
    // than discovering it from the watchdog minutes later.
    let (watch_tx, mut watch_rx) = mpsc::channel::<Option<ChannelId>>(8);

    tokio::spawn(async move {
        let base = vec![
            Topic::UserDrops.as_str(&user_id),
            Topic::UserPoints.as_str(&user_id),
        ];
        let mut backoff = StdDuration::from_secs(1);
        // Survives reconnection, so the subscription is restored automatically.
        let mut followed: Option<ChannelId> = None;

        loop {
            match pubsub::connect(pubsub::PUBSUB_URL, &token, base.clone()).await {
                Ok(mut socket) => {
                    tracing::info!("pubsub connected");
                    backoff = StdDuration::from_secs(1);

                    if let Some(id) = &followed {
                        socket.listen(vec![Topic::StreamState.as_str(&id.0)]).await;
                    }

                    loop {
                        tokio::select! {
                            change = watch_rx.recv() => {
                                let Some(next) = change else { return };
                                if next == followed {
                                    continue;
                                }
                                if let Some(old) = &followed {
                                    socket
                                        .unlisten(vec![Topic::StreamState.as_str(&old.0)])
                                        .await;
                                }
                                if let Some(new) = &next {
                                    socket
                                        .listen(vec![Topic::StreamState.as_str(&new.0)])
                                        .await;
                                }
                                followed = next;
                            }
                            msg = socket.recv() => {
                                let Some(msg) = msg else { break };
                                if matches!(msg, PubSubMessage::Disconnected) {
                                    break;
                                }
                                // The consumer going away means the miner is
                                // shutting down; stop rather than reconnecting
                                // into the void.
                                if tx.send(msg).await.is_err() {
                                    return;
                                }
                            }
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

    (rx, watch_tx)
}

async fn handle_pubsub<A: TwitchApi>(engine: &mut Engine<A>, msg: PubSubMessage) {
    let now = Utc::now();
    match msg {
        PubSubMessage::DropProgress(p) => engine.on_pushed_progress(p, now),
        PubSubMessage::DropClaimable { drop_id, claim_id } => {
            tracing::info!(drop = %drop_id, "drop is claimable");
            engine.on_drop_claimable(&drop_id, &claim_id);
            // Claim at once rather than waiting for the next tick: a claim is
            // the one thing the user actually wants, and there is no reason to
            // hold it for up to a minute.
            if let Err(e) = engine.claim_ready(now).await {
                tracing::warn!(error = %e, "claiming failed; will retry on the next tick");
            }
        }
        PubSubMessage::StreamDown { channel_id } => {
            if engine.current_channel().is_some_and(|c| c.id == channel_id) {
                tracing::info!(channel = %channel_id, "watched channel went offline");
                engine.stop_watching(now, btdm_core::event::StopReason::WentOffline);
            }
        }
        PubSubMessage::PointsClaimable {
            channel_id,
            claim_id,
        } => {
            // Claiming is best-effort: points are a bonus, never a reason to
            // interrupt mining.
            if let Err(e) = engine.claim_points(&channel_id, &claim_id, now).await {
                tracing::debug!(error = %e, "channel points claim failed");
            }
        }
        PubSubMessage::StreamUp { .. } | PubSubMessage::Disconnected => {}
    }
}

async fn drain_events<A: TwitchApi>(
    engine: &mut Engine<A>,
    storage: &Storage,
    account: &str,
    hub: Option<&Hub>,
    notifier: Option<&Discord>,
) -> anyhow::Result<()> {
    for event in engine.drain_events() {
        tracing::info!(summary = %event.summary(), "event");
        storage.append_event(account, &event)?;
        if let Some(notifier) = notifier {
            notifier.notify(account, &event).await;
        }
        if let Some(hub) = hub {
            hub.emit(account, event);
        }
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
        MiningState::Paused => tracing::info!("paused"),
        MiningState::Idle => tracing::info!("idle — no eligible channel"),
        MiningState::Establishing => tracing::info!(
            channel,
            last_confirmed = since,
            "watching, awaiting first confirmation"
        ),
        MiningState::Mining => {
            let campaigns = engine.current().map(|w| w.campaigns.len()).unwrap_or(0);
            tracing::info!(
                channel,
                campaigns,
                confirmed = progress,
                last_confirmed = since,
                "mining"
            )
        }
    }
}
