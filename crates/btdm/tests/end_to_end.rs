//! The whole miner, against a fake Twitch.
//!
//! Everything else tests a layer. This tests the wiring: `run::mine` starting
//! from a stored token, validating it, syncing campaigns, choosing a channel,
//! playing the stream and sending a heartbeat to the spade endpoint it had to
//! discover by scraping a page, polling progress and claiming a finished drop —
//! the sequence that decides what actually happens on a real account, and
//! which was previously verified only by running the binary and watching the
//! log.
//!
//! The fake speaks enough of Twitch's private API to be indistinguishable from
//! it for one pass: OAuth validation, batched and single GraphQL, the mobile
//! page that embeds the spade URL, spade itself, and the playlists and video
//! segments of a live stream.

use btdm::config::Config;
use btdm::run::{self, RunOptions};
use btdm::storage::{Storage, Tokens};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the fake Twitch saw.
#[derive(Default)]
struct Seen {
    heartbeats: AtomicUsize,
    /// Video segments requested. Since 2026-10-08 these, not heartbeats, are
    /// what Twitch credits drop time for.
    segments: AtomicUsize,
    claims: AtomicUsize,
    operations: std::sync::Mutex<Vec<String>>,
    /// Answer the progress poll with a gateway error instead of a session.
    /// Twitch really does this, and what the miner did about it mattered far
    /// more than the error itself.
    break_progress_poll: std::sync::atomic::AtomicBool,
    /// How many heartbeats to answer with a gateway error before behaving.
    break_heartbeats: AtomicUsize,
}

/// A minimal Twitch. Returns its base URL.
async fn fake_twitch(seen: Arc<Seen>, drop_minutes: u32) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let base_for_task = base.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            let base = base_for_task.clone();

            tokio::spawn(async move {
                let mut buf = vec![0u8; 262_144];
                let n = match socket.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                let method = raw.split_whitespace().next().unwrap_or("").to_string();
                let path = raw.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();

                let (status, content_type, payload) =
                    respond(&method, &path, &body, &seen, &base, drop_minutes);
                let payload = if method == "HEAD" { String::new() } else { payload };

                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    base
}

fn respond(
    method: &str,
    path: &str,
    body: &str,
    seen: &Seen,
    base: &str,
    drop_minutes: u32,
) -> (u16, &'static str, String) {
    let spade = format!("{base}/spade");
    if path.starts_with("/oauth2/rejected") {
        return (401, "application/json", json!({ "message": "invalid" }).to_string());
    }

    if path.starts_with("/oauth2/validate") {
        return (
            200,
            "application/json",
            json!({ "client_id": "x", "login": "tester", "user_id": "1", "expires_in": 0 })
                .to_string(),
        );
    }

    // The mobile channel page, which embeds the spade URL.
    if path.starts_with("/streamer") {
        return (
            200,
            "text/html",
            format!(r#"<html><script>{{"spade_url":"{spade}"}}</script></html>"#),
        );
    }

    // A live stream: its playlist index, its playlist, and its video.
    if path.starts_with("/api/channel/hls/") {
        return (
            200,
            "application/vnd.apple.mpegurl",
            format!("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=200000\n{base}/media/streamer.m3u8\n"),
        );
    }
    if path.starts_with("/media/") {
        let mut playlist = String::from("#EXTM3U\n");
        for i in 0..3 {
            playlist.push_str(&format!("#EXTINF:2.000,live\n{base}/segment/{i}.ts\n"));
        }
        return (200, "application/vnd.apple.mpegurl", playlist);
    }
    if method == "HEAD" && path.starts_with("/segment/") {
        seen.segments.fetch_add(1, Ordering::SeqCst);
        return (200, "video/mp2t", String::new());
    }

    // The minute-watched event. No longer what earns drop time, but still
    // sent, as a web player does.
    if path.starts_with("/spade") {
        // Counted only when accepted: the point of the test below is that a
        // refused heartbeat is not a heartbeat.
        if seen
            .break_heartbeats
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return (504, "text/plain", String::new());
        }
        seen.heartbeats.fetch_add(1, Ordering::SeqCst);
        return (204, "text/plain", String::new());
    }

    if path.starts_with("/gql") {
        if body.contains("DropCurrentSessionContext")
            && seen.break_progress_poll.load(Ordering::SeqCst)
        {
            return (504, "text/plain", String::new());
        }
        return (200, "application/json", gql(body, seen, drop_minutes));
    }

    (404, "text/plain", "no".into())
}

/// Answers one GraphQL request, batched or not.
fn gql(body: &str, seen: &Seen, drop_minutes: u32) -> String {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    match &parsed {
        Value::Array(ops) => {
            let answers: Vec<Value> = ops.iter().map(|o| answer(o, seen, drop_minutes)).collect();
            Value::Array(answers).to_string()
        }
        other => answer(other, seen, drop_minutes).to_string(),
    }
}

fn answer(op: &Value, seen: &Seen, drop_minutes: u32) -> Value {
    let name = op
        .get("operationName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    seen.operations.lock().unwrap().push(name.clone());

    let drop = json!({
        "id": "d1",
        "name": "Test Drop",
        "requiredMinutesWatched": 60,
        "benefitEdges": [{ "benefit": { "id": "b1", "name": "A Reward" } }],
        "self": {
            "currentMinutesWatched": drop_minutes,
            "isClaimed": false,
            "dropInstanceID": if drop_minutes >= 60 { json!("claim-1") } else { Value::Null },
        },
    });
    let campaign = json!({
        "id": "c1",
        "name": "Test Campaign",
        "status": "ACTIVE",
        "startAt": "2020-01-01T00:00:00Z",
        "endAt": "2099-01-01T00:00:00Z",
        "game": { "id": "g1", "name": "Test Game", "slug": "test-game" },
        "self": { "isAccountConnected": true },
        "allow": { "channels": Value::Null },
        "timeBasedDrops": [drop],
    });

    let data = match name.as_str() {
        "Inventory" => json!({
            "currentUser": {
                "inventory": {
                    "dropCampaignsInProgress": [campaign],
                    "gameEventDrops": [],
                }
            }
        }),
        "ViewerDropsDashboard" => json!({ "currentUser": { "dropCampaigns": [] } }),
        "DirectoryPage_Game" => json!({
            "game": {
                "streams": {
                    "edges": [{
                        "node": {
                            "id": "bc-1",
                            "title": "streaming",
                            "viewersCount": 100,
                            "broadcaster": {
                                "id": "ch1",
                                "login": "streamer",
                                "displayName": "Streamer",
                            },
                            "game": { "id": "g1", "name": "Test Game", "slug": "test-game" },
                            "freeformTags": [{ "name": "DropsEnabled" }],
                        }
                    }]
                }
            }
        }),
        // What Twitch actually returns today: only sitewide reward campaigns,
        // never the game campaign the channel is visibly earning. Answering
        // with `c1` here would test a Twitch that no longer exists, and is
        // precisely why nothing caught the miner selecting channels
        // alphabetically for hours.
        "DropsHighlightService_AvailableDrops" => json!({
            "channel": {
                "viewerDropCampaigns": [{
                    "id": "sitewide-reward",
                    "timeBasedDrops": [{ "id": "unrelated-drop" }],
                }]
            }
        }),
        "VideoPlayerStreamInfoOverlayChannel" => json!({
            "user": {
                "id": "ch1",
                "login": "streamer",
                "displayName": "Streamer",
                "stream": {
                    "id": "bc-1",
                    "game": { "id": "g1", "name": "Test Game", "slug": "test-game" },
                },
                "broadcastSettings": {
                    "title": "streaming",
                    "game": { "id": "g1", "name": "Test Game", "slug": "test-game" },
                },
            }
        }),
        "DropCurrentSessionContext" => json!({
            "currentUser": {
                "dropCurrentSession": {
                    "channel": { "id": "ch1" },
                    "dropID": "d1",
                    "currentMinutesWatched": drop_minutes,
                    "requiredMinutesWatched": 60,
                }
            }
        }),
        "PlaybackAccessToken" => json!({
            "streamPlaybackAccessToken": { "value": "{\"channel\":\"streamer\"}", "signature": "sig" }
        }),
        "DropsPage_ClaimDropRewards" => {
            seen.claims.fetch_add(1, Ordering::SeqCst);
            json!({ "claimDropRewards": { "status": "ELIGIBLE_FOR_ALL" } })
        }
        _ => json!({}),
    };

    json!({ "data": data })
}

/// A miner pointed at the fake, with a token already stored.
fn miner(base: &str) -> (Storage, Config, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(&dir.path().join("state.db")).unwrap();
    storage
        .put_tokens(
            "default",
            &Tokens {
                access_token: "test-token".into(),
                refresh_token: None,
                user_id: "1".into(),
                login: "tester".into(),
                device_id: "device".into(),
            },
        )
        .unwrap();

    point_at(base);
    (storage, Config::default(), dir)
}

fn point_at(base: &str) {
    // SAFETY: set before any client is built, and these tests are serialised
    // by the lock below because the variables are process-global.
    unsafe {
        std::env::set_var("BTDM_GQL_URL", format!("{base}/gql"));
        std::env::set_var("BTDM_VALIDATE_URL", format!("{base}/oauth2/validate"));
        std::env::set_var("BTDM_MOBILE_URL", base);
        std::env::set_var("BTDM_USHER_URL", base);
        // Nothing listens for websockets. The supervisor retries in the
        // background and the poll backstop carries progress, which is itself
        // worth proving works.
        std::env::set_var("BTDM_PUBSUB_URL", "ws://127.0.0.1:1/nope");
    }
}

/// Runs one scenario with the endpoint overrides to itself.
///
/// They are process-global, so two tests pointing at two fakes at once would
/// each count the other's requests. The lock is held for the whole body rather
/// than awaited, which is what makes that impossible rather than unlikely.
fn serial<F>(body: F)
where
    F: std::future::Future<Output = ()>,
{
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body);
}

async fn probe(
    storage: &Storage,
    config: &Config,
    account: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    run::mine(
        storage,
        account,
        config,
        RunOptions {
            probe: true,
            config_store: None,
        },
        None,
    )
    .await
    .map_err(Into::into)
}

/// One full pass: sync, select, heartbeat, poll.
#[test]
fn the_miner_syncs_selects_and_sends_a_heartbeat() {
    serial(async {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen), 5).await;
    let (storage, config, _dir) = miner(&base);

    probe(&storage, &config, "default")
        .await
        .expect("a probe against a working Twitch must succeed");

    let ops = seen.operations.lock().unwrap().clone();
    assert!(ops.contains(&"Inventory".to_string()), "never synced: {ops:?}");
    assert!(
        ops.contains(&"DirectoryPage_Game".to_string()),
        "never looked for a channel: {ops:?}"
    );
    assert!(
        ops.contains(&"DropsHighlightService_AvailableDrops".to_string()),
        "never verified the channel earns anything: {ops:?}"
    );
    assert_eq!(
        seen.segments.load(Ordering::SeqCst),
        3,
        "the stream was not played: since 2026-10-08 nothing else earns drop time"
    );
    assert_eq!(
        seen.heartbeats.load(Ordering::SeqCst),
        1,
        "the minute-watched event was not sent"
    );
    });
}

/// The event log is what the dashboard and the user read afterwards.
#[test]
fn a_pass_records_what_it_did() {
    serial(async {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen), 5).await;
    let (storage, config, _dir) = miner(&base);

    probe(&storage, &config, "default").await.unwrap();

    let events = storage.recent_events("default", 100).unwrap();
    let summaries: Vec<String> = events.iter().map(|e| e.summary.clone()).collect();
    assert!(
        summaries.iter().any(|s| s.contains("Synced")),
        "the sync was not recorded: {summaries:?}"
    );
    assert!(
        summaries.iter().any(|s| s.contains("Watching Streamer")),
        "the choice was not recorded: {summaries:?}"
    );
    assert!(
        summaries.iter().any(|s| s.contains("Heartbeat sent")),
        "the heartbeat was not recorded: {summaries:?}"
    );
    });
}

/// A finished drop must be claimed, and recorded as claimed.
#[test]
fn a_finished_drop_is_claimed_end_to_end() {
    serial(async {
    let seen = Arc::new(Seen::default());
    // Already past the requirement, with a claim id waiting.
    let base = fake_twitch(Arc::clone(&seen), 60).await;
    let (storage, config, _dir) = miner(&base);

    probe(&storage, &config, "default").await.unwrap();

    assert_eq!(
        seen.claims.load(Ordering::SeqCst),
        1,
        "an earned drop went unclaimed"
    );
    let claimed = storage.claimed_drops("default", 10).unwrap();
    assert_eq!(claimed.len(), 1, "the claim was not recorded");
    assert_eq!(claimed[0].name, "Test Drop");
    });
}

/// A token Twitch rejects must stop with an actionable message rather than
/// leaving the miner retrying forever.
#[test]
fn a_rejected_token_stops_with_an_explanation() {
    serial(async {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen), 5).await;
    let (storage, config, _dir) = miner(&base);

    // Twitch answers a dead token with 401.
    unsafe {
        std::env::set_var("BTDM_VALIDATE_URL", format!("{base}/oauth2/rejected"));
    }

    let err = probe(&storage, &config, "default")
        .await
        .expect_err("a rejected token cannot be mined through");

    let text = format!("{err:#}").to_lowercase();
    assert!(
        text.contains("auth") || text.contains("401") || text.contains("valid"),
        "the reason was not actionable: {text}"
    );

    point_at(&base);
    });
}

/// An account with no stored token must say so rather than starting.
#[test]
fn an_unlinked_account_is_refused_with_instructions() {
    serial(async {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen), 5).await;
    let (storage, config, _dir) = miner(&base);

    let err = probe(&storage, &config, "never-linked")
        .await
        .expect_err("there is no token for that account");

    assert!(
        format!("{err}").contains("btdm login"),
        "an error without a next step is just noise: {err}"
    );
    });
}

/// A miner whose configured strategy does not exist must still mine, on the
/// default, rather than refusing to start.
#[test]
fn an_unknown_configured_strategy_falls_back_rather_than_failing() {
    serial(async {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen), 5).await;
    let (storage, mut config, _dir) = miner(&base);
    config.accounts[0].strategy = "telepathy".into();

    probe(&storage, &config, "default").await.unwrap();
    assert_eq!(
        seen.heartbeats.load(Ordering::SeqCst),
        1,
        "a typo in the config must not stop mining"
    );
    });
}

/// The whole binary, against a Twitch that confirms nothing.
///
/// This is the shape production actually returns: `viewerDropCampaigns` names
/// only a sitewide reward and never the game campaign the channel is earning.
/// The miner must still choose a channel on merit and send a heartbeat for it.
/// It did not — it fell through to a path that ignored the strategy entirely
/// and took whichever channel sorted first — and no test noticed, because the
/// fake above used to answer with exactly the drop the test wanted.
#[test]
fn the_miner_mines_a_twitch_that_confirms_nothing() {
    serial(async {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen), 5).await;
    let (storage, config, _dir) = miner(&base);

    probe(&storage, &config, "default")
        .await
        .expect("a pass must complete");

    assert_eq!(
        seen.heartbeats.load(Ordering::SeqCst),
        1,
        "the miner must watch something even though Twitch confirmed nothing"
    );
    });
}

/// Observed live on 2026-08-27. Three HTTP 504s in ten minutes, every one of
/// them on the progress poll, and every one answered by tearing down a
/// perfectly good watch session: "Stopped watching — an error made the session
/// untrustworthy", then "idle — no eligible channel", then a fresh selection
/// pass costing minutes. Roughly half of that ten-minute window was spent idle
/// over three failed requests, on a channel that was live and earning the
/// whole time.
///
/// The poll is the *backstop* for a websocket that is connected but silent. It
/// failing says nothing whatever about the channel, and the session it was
/// checking on is exactly what must survive it.
#[test]
fn a_gateway_error_on_the_progress_poll_does_not_cost_the_channel() {
    serial(async {
        let seen = Arc::new(Seen::default());
        seen.break_progress_poll.store(true, Ordering::SeqCst);
        let base = fake_twitch(Arc::clone(&seen), 5).await;
        let (storage, config, _dir) = miner(&base);
        let hub = btdm_web::Hub::new();

        run::mine(
            &storage,
            "default",
            &config,
            RunOptions { probe: true, config_store: None },
            Some(hub.clone()),
        )
        .await
        .expect("a failed poll is not a reason to stop mining");

        assert_eq!(
            seen.heartbeats.load(Ordering::SeqCst),
            1,
            "the heartbeat landed; only the poll after it failed"
        );

        let snapshot = hub.snapshot("default").expect("the miner published its state");
        assert!(
            snapshot.watching.is_some(),
            "the channel was live and being watched; a failed backstop poll must \
             not throw the session away, state was {:?}",
            snapshot.state
        );
        assert_ne!(
            snapshot.state, "Idle",
            "reporting idle here tells the user nobody could be mined, while a \
             channel that could be was just given up"
        );
    })
}

/// The minute-watched event was once the one request in the miner with no
/// retry at all, and back then the only one that advanced drop time. Twitch
/// was observed answering 504 freely on 2026-08-27. It no longer earns
/// anything (see `btdm_twitch::playback`), but it is still sent, and a blip
/// should not be what stops it arriving.
#[test]
fn a_flaky_spade_endpoint_still_gets_the_event_through() {
    serial(async {
        let seen = Arc::new(Seen::default());
        seen.break_heartbeats.store(2, Ordering::SeqCst);
        let base = fake_twitch(Arc::clone(&seen), 5).await;
        let (storage, config, _dir) = miner(&base);

        probe(&storage, &config, "default")
            .await
            .expect("two gateway errors are not a reason to stop mining");

        assert_eq!(
            seen.heartbeats.load(Ordering::SeqCst),
            1,
            "the third attempt must land"
        );
    })
}

/// And the bound, because retrying forever would hold up the heartbeat.
///
/// Since 2026-10-08 spade earns nothing, so it failing outright must not make
/// a heartbeat that played the stream look failed: that would report a
/// working session as broken.
#[test]
fn heartbeat_retries_are_bounded() {
    serial(async {
        let seen = Arc::new(Seen::default());
        seen.break_heartbeats.store(usize::MAX, Ordering::SeqCst);
        let base = fake_twitch(Arc::clone(&seen), 5).await;
        let (storage, config, _dir) = miner(&base);

        probe(&storage, &config, "default")
            .await
            .expect("a failed heartbeat is logged, not fatal");

        assert_eq!(
            seen.heartbeats.load(Ordering::SeqCst),
            0,
            "none was ever accepted, and the pass still ended"
        );
        assert!(seen.segments.load(Ordering::SeqCst) > 0, "the stream was not played");
        let summaries: Vec<String> = storage
            .recent_events("default", 100)
            .unwrap()
            .iter()
            .map(|e| e.summary.clone())
            .collect();
        assert!(
            summaries.iter().any(|s| s.contains("Heartbeat sent")),
            "the stream played, yet the heartbeat was recorded as failed: {summaries:?}"
        );
    })
}
