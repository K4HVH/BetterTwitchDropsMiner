//! The player, against a real socket.
//!
//! Since 2026-10-08 these requests are what earns drop time, so what they do
//! when the stream moves on, the token lapses or the miner changes its mind is
//! checked here rather than discovered on a live account a day of drops later.

use btdm_core::model::{Channel, ChannelId, Stream};
use btdm_twitch::gql::GqlClient;
use btdm_twitch::playback::Player;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How often the player polls in these tests. Production uses seconds.
const POLL: Duration = Duration::from_millis(40);

/// What the fake Twitch saw.
#[derive(Default)]
struct Seen {
    token_requests: AtomicUsize,
    /// Media playlist fetches per channel login.
    playlist_fetches: Mutex<HashMap<String, usize>>,
    /// Every segment HEAD, as `login/index`.
    heads: Mutex<Vec<String>>,
    /// Refuse this many media playlist fetches with a 403, as Twitch does once
    /// a playback token has lapsed.
    refuse_playlist: AtomicUsize,
    /// Hold each media playlist answer this long, in milliseconds.
    slow_playlist_ms: AtomicUsize,
    /// Answer token requests after this many with a 401, as Twitch does for a
    /// revoked login. Zero means never.
    revoke_after: AtomicUsize,
}

impl Seen {
    fn fetches(&self, login: &str) -> usize {
        self.playlist_fetches.lock().unwrap().get(login).copied().unwrap_or(0)
    }

    fn heads_for(&self, login: &str) -> Vec<usize> {
        self.heads
            .lock()
            .unwrap()
            .iter()
            .filter_map(|h| h.strip_prefix(&format!("{login}/")))
            .map(|i| i.parse().unwrap())
            .collect()
    }
}

/// A Twitch serving playback for any login except `offline`. Each media
/// playlist fetch slides a three-segment window forward by one, like a live
/// stream does. Returns the base URL.
async fn fake_twitch(seen: Arc<Seen>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let base_for_task = base.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            let base = base_for_task.clone();
            tokio::spawn(async move {
                let Some(raw) = read_request(&mut socket).await else {
                    return;
                };
                let delay = seen.slow_playlist_ms.load(Ordering::SeqCst);
                if delay > 0 && raw.contains(" /media/") {
                    tokio::time::sleep(Duration::from_millis(delay as u64)).await;
                }
                let mut words = raw.split_whitespace();
                let method = words.next().unwrap_or("").to_owned();
                let path = words.next().unwrap_or("/").to_owned();
                let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();

                let (status, payload) = respond(&method, &path, &body, &seen, &base);
                let length = payload.len();
                let payload = if method == "HEAD" { String::new() } else { payload };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{payload}"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    base
}

/// One whole request: the headers, then as much body as they announced. A
/// body can arrive in a later packet than its headers.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = socket.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&raw).to_string();
        let Some(split) = text.find("\r\n\r\n") else {
            continue;
        };
        let length = text[..split]
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0usize);
        if raw.len() >= split + 4 + length {
            return Some(text);
        }
    }
}

fn respond(method: &str, path: &str, body: &str, seen: &Seen, base: &str) -> (u16, String) {
    if path.starts_with("/gql") {
        let n = seen.token_requests.fetch_add(1, Ordering::SeqCst) + 1;
        let revoke_after = seen.revoke_after.load(Ordering::SeqCst);
        if revoke_after > 0 && n > revoke_after {
            return (401, r#"{"error":"Unauthorized","status":401}"#.into());
        }
        let request: serde_json::Value = serde_json::from_str(body).unwrap();
        let login = request["variables"]["login"].as_str().unwrap().to_owned();
        let token = serde_json::json!({ "channel": login }).to_string();
        let data = serde_json::json!({
            "data": { "streamPlaybackAccessToken": { "value": token, "signature": "sig" } }
        });
        return (200, data.to_string());
    }

    if let Some(rest) = path.strip_prefix("/api/channel/hls/") {
        let login = rest.split(".m3u8").next().unwrap();
        if login == "offline" {
            return (
                404,
                r#"[{"error":"twirp error not_found: transcode does not exist","error_code":"transcode_does_not_exist"}]"#.into(),
            );
        }
        assert!(path.contains("sig=sig"), "the playlist index needs the token: {path}");
        return (
            200,
            format!(
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=900000\n{base}/hi/{login}.m3u8\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=200000\n{base}/media/{login}.m3u8\n"
            ),
        );
    }

    if let Some(rest) = path.strip_prefix("/media/") {
        if seen
            .refuse_playlist
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return (403, "forbidden".into());
        }
        let login = rest.trim_end_matches(".m3u8").to_owned();
        let first = {
            let mut fetches = seen.playlist_fetches.lock().unwrap();
            let n = fetches.entry(login.clone()).or_insert(0);
            *n += 1;
            *n - 1
        };
        let mut playlist = format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
        for i in first..first + 3 {
            playlist.push_str(&format!("#EXTINF:2.000,live\n{base}/seg/{login}/{i}.ts\n"));
        }
        return (200, playlist);
    }

    if method == "HEAD"
        && let Some(rest) = path.strip_prefix("/seg/")
    {
        seen.heads.lock().unwrap().push(rest.trim_end_matches(".ts").to_owned());
        return (200, "x".repeat(1000));
    }

    (404, "no".into())
}

fn channel(login: &str) -> Channel {
    Channel {
        id: ChannelId::from(format!("id-{login}").as_str()),
        login: login.into(),
        display_name: login.into(),
    }
}

fn stream(broadcast: &str) -> Stream {
    Stream {
        broadcast_id: broadcast.into(),
        game: None,
        title: String::new(),
        viewers: 1,
        drops_enabled: true,
    }
}

fn player(base: &str) -> Player {
    let http = reqwest::Client::new();
    let gql = GqlClient::with_endpoint(http.clone(), Some("token".into()), format!("{base}/gql"));
    Player::new(http, Arc::new(gql), base.to_owned(), POLL)
}

/// Waits for `done`, or fails the test after a couple of seconds.
async fn until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// The whole point: every segment the stream serves is asked for, none twice.
/// A gap is a stretch of stream the account was not watching; a repeat is a
/// request that bought nothing.
#[tokio::test]
async fn every_segment_is_requested_exactly_once() {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = player(&base);

    player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await
        .expect("a live stream can be watched");
    // Seven is the first window plus four slides of one.
    until("the window to slide four times", || seen.heads_for("alpha").len() >= 7).await;
    player.stop();

    let mut heads = seen.heads_for("alpha");
    let total = heads.len();
    heads.sort_unstable();
    heads.dedup();
    assert_eq!(heads.len(), total, "a segment was requested twice: {heads:?}");
    let expected: Vec<usize> = (0..heads.len()).collect();
    assert_eq!(heads, expected, "a segment was skipped");
}

/// A playback token lasts twenty minutes. After that the playlist is refused,
/// and a player that kept asking with the old one would be watching nothing
/// for the rest of the stream.
#[tokio::test]
async fn a_refused_playlist_is_retried_with_a_fresh_token() {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = player(&base);

    player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await
        .unwrap();
    assert_eq!(seen.token_requests.load(Ordering::SeqCst), 1);

    seen.refuse_playlist.store(1, Ordering::SeqCst);
    let before = seen.heads_for("alpha").len();
    until("a new token and new segments", || {
        seen.token_requests.load(Ordering::SeqCst) >= 2 && seen.heads_for("alpha").len() > before
    })
    .await;
    player.stop();
}

/// Stopping is how leaving a channel and pausing take effect. A player left
/// running would go on watching a channel the miner had already given up.
#[tokio::test]
async fn stopping_ends_the_requests() {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = player(&base);

    player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await
        .unwrap();
    until("a second poll", || seen.fetches("alpha") >= 2).await;
    player.stop();
    // One poll may already be in flight when stop lands.
    tokio::time::sleep(POLL).await;
    let after_stop = seen.fetches("alpha");

    tokio::time::sleep(POLL * 8).await;
    assert_eq!(seen.fetches("alpha"), after_stop, "still polling after stop");
}

/// The same when the whole client goes away with its account.
#[tokio::test]
async fn dropping_the_player_ends_the_requests() {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = player(&base);

    player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await
        .unwrap();
    drop(player);
    tokio::time::sleep(POLL).await;
    let after_drop = seen.fetches("alpha");

    tokio::time::sleep(POLL * 8).await;
    assert_eq!(seen.fetches("alpha"), after_drop, "still polling after drop");
}

/// One account watches one stream. Switching must leave the old one, or the
/// account is split across two channels and the new one is not the only
/// stream being served.
#[tokio::test]
async fn watching_another_stream_leaves_the_first() {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = player(&base);

    player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await
        .unwrap();
    player
        .keep_watching(&channel("beta"), &stream("b2"))
        .await
        .unwrap();
    tokio::time::sleep(POLL).await;
    let alpha = seen.fetches("alpha");

    until("beta to be polled", || seen.fetches("beta") >= 4).await;
    assert_eq!(seen.fetches("alpha"), alpha, "the first stream is still being watched");
    player.stop();
}

/// A restarted broadcast is a new stream with a new playlist; the old
/// player's segments say nothing about it.
#[tokio::test]
async fn a_new_broadcast_on_the_same_channel_starts_over() {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = player(&base);

    player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await
        .unwrap();
    player
        .keep_watching(&channel("alpha"), &stream("b2"))
        .await
        .unwrap();
    assert_eq!(
        seen.token_requests.load(Ordering::SeqCst),
        2,
        "the new broadcast was not resolved afresh"
    );
    player.stop();
}

/// Asking again about the same stream is a heartbeat, not a restart: it must
/// not fetch a new token or replay segments already requested.
#[tokio::test]
async fn a_repeated_heartbeat_reports_without_restarting() {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = player(&base);

    player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await
        .unwrap();
    until("a few polls", || seen.fetches("alpha") >= 3).await;
    player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await
        .expect("video has been served since the last heartbeat");
    assert_eq!(seen.token_requests.load(Ordering::SeqCst), 1, "restarted");
    player.stop();
}

/// An offline channel serves no video. Saying the heartbeat worked would be
/// the exact invisible failure this project exists to prevent.
#[tokio::test]
async fn a_stream_serving_nothing_is_reported_as_a_failure() {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = player(&base);

    let first = player
        .keep_watching(&channel("offline"), &stream("b1"))
        .await;
    let shown = btdm_core::api::ApiError::from(
        first.expect_err("nothing was served, yet it reported success"),
    )
    .to_string();
    // Twitch answered; it had no stream to give. Calling that a network
    // failure would name a cause nobody established.
    assert!(!shown.contains("network"), "{shown}");
    assert!(shown.contains("404"), "what Twitch actually said is lost: {shown}");

    tokio::time::sleep(POLL * 3).await;
    let later = player
        .keep_watching(&channel("offline"), &stream("b1"))
        .await;
    assert!(later.is_err(), "still nothing served, yet it reported success");
    player.stop();
}

/// A stop that lands while a heartbeat is still on its first request must
/// still win. Otherwise the player it stopped starts anyway once that request
/// returns, and goes on watching a channel the miner has already left.
#[tokio::test]
async fn stopping_during_the_first_poll_still_stops() {
    let seen = Arc::new(Seen::default());
    seen.slow_playlist_ms.store(150, Ordering::SeqCst);
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = Arc::new(player(&base));

    let starting = {
        let player = Arc::clone(&player);
        tokio::spawn(async move {
            player
                .keep_watching(&channel("alpha"), &stream("b1"))
                .await
        })
    };
    until("the first poll to begin", || {
        seen.token_requests.load(Ordering::SeqCst) >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    player.stop();
    let _ = starting.await.unwrap();

    let after_stop = seen.fetches("alpha");
    tokio::time::sleep(POLL * 8).await;
    assert_eq!(seen.fetches("alpha"), after_stop, "the stopped player started anyway");
}

/// A first poll that fails is not the end of watching: the player keeps
/// trying on its own clock, and the next heartbeat reports that it recovered.
#[tokio::test]
async fn a_failed_start_recovers_by_the_next_heartbeat() {
    let seen = Arc::new(Seen::default());
    seen.refuse_playlist.store(1, Ordering::SeqCst);
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = player(&base);

    let first = player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await;
    assert!(first.is_err(), "the playlist was refused, yet it reported success");

    // Polls run one after another, so a second playlist fetch means the poll
    // that served video has finished and recorded it.
    until("a poll to serve video and finish", || seen.fetches("alpha") >= 2).await;
    player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await
        .expect("video has been served since, so the heartbeat worked");
    player.stop();
}

/// A login revoked while the player runs in the background has to reach the
/// heartbeat as exactly that. It is how the account learns to stop and ask
/// for a new one, rather than retrying a dead token every ten seconds.
#[tokio::test]
async fn a_revoked_token_reaches_the_heartbeat() {
    let seen = Arc::new(Seen::default());
    let base = fake_twitch(Arc::clone(&seen)).await;
    let player = player(&base);

    player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await
        .unwrap();
    seen.revoke_after.store(1, Ordering::SeqCst);
    seen.refuse_playlist.store(1, Ordering::SeqCst);

    until("a renewal to be refused twice", || {
        seen.token_requests.load(Ordering::SeqCst) >= 3
    })
    .await;
    // Whatever was served before the revocation is reported here and done with.
    let _ = player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await;
    // Two more renewals: polls run one after another, so the second starting
    // means the first, refused after that report, has been recorded.
    let asked = seen.token_requests.load(Ordering::SeqCst);
    until("a refused renewal after the report", || {
        seen.token_requests.load(Ordering::SeqCst) >= asked + 2
    })
    .await;

    let after = player
        .keep_watching(&channel("alpha"), &stream("b1"))
        .await;
    assert!(
        matches!(after, Err(btdm_twitch::TwitchError::Unauthorized)),
        "{after:?}"
    );
    player.stop();
}
