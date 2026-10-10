//! Watching, as Twitch counts it.
//!
//! Until 2026-10-08 a `minute-watched` event posted to spade was enough. Since
//! then Twitch credits drop time only to an account that is actually being
//! served the stream. On this miner's own account on 2026-10-09, an R6S drop
//! crept from 3 to 6 of 60 minutes over eight hours of repeated visits, every
//! spade event accepted. The next day, with nothing else changed, HEAD
//! requests for a stream's video segments took a campaign from no session at
//! all to 4 minutes in under five. Other miners measured the same on
//! 2026-10-08.
//!
//! So this plays the stream the way a player does, minus the video: a
//! playback token, the stream's playlist, and a HEAD for each new segment,
//! often enough that none scrolls out of the playlist unseen. The segments'
//! bodies are never downloaded.

use crate::gql::GqlClient;
use crate::operations as ops;
use crate::{TwitchError, describe, parse};
use btdm_core::model::{Channel, ChannelId, Stream};
use reqwest::Url;
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::time::Instant;

/// Where a stream's playlist index lives.
pub const USHER_URL: &str = "https://usher.ttvnw.net";

/// How often the playlist is read. A live playlist lists about thirty seconds
/// of video, so ten seconds leaves room for a slow poll before a segment
/// scrolls out of it unrequested.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// A playback token lasts twenty minutes. Renewing at ten stays clear of the
/// edge; a refused playlist renews it sooner anyway.
const RENEW_AFTER: Duration = Duration::from_secs(600);

const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(5);
const SEGMENT_TIMEOUT: Duration = Duration::from_secs(3);

/// How many requested segments are remembered. A playlist lists a few dozen
/// at most; the rest is margin for a CDN briefly serving an older window.
const REMEMBERED_SEGMENTS: usize = 256;

/// Keeps one stream playing in the background for as long as the engine says
/// it is watching it.
///
/// The engine's heartbeat runs once a minute, which is too slow: a minute
/// between polls leaves half of every playlist unrequested. So the first
/// heartbeat for a stream starts a player that polls on its own clock, and
/// later ones only ask it how it has been doing.
pub struct Player {
    http: reqwest::Client,
    gql: Arc<GqlClient>,
    usher: String,
    every: Duration,
    playing: Mutex<Option<Playing>>,
    /// Bumped by every start and every stop. A start whose first poll was
    /// overtaken by either installs nothing: a stop that arrives while that
    /// poll is still out must win.
    generation: AtomicU64,
}

struct Playing {
    channel: ChannelId,
    broadcast_id: String,
    health: Arc<Mutex<Health>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Playing {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Player {
    pub fn new(http: reqwest::Client, gql: Arc<GqlClient>, usher: String, every: Duration) -> Self {
        Self {
            http,
            gql,
            usher,
            every,
            playing: Mutex::new(None),
            generation: AtomicU64::new(0),
        }
    }

    /// Watches this stream, and says whether it has actually served video
    /// since the last time this was asked.
    ///
    /// A different channel or broadcast from last time leaves the old one
    /// first: one account watching two streams is not what Twitch is told.
    pub async fn keep_watching(&self, channel: &Channel, stream: &Stream) -> Result<(), TwitchError> {
        {
            let mut playing = lock(&self.playing);
            if let Some(p) = playing.as_ref()
                && p.channel == channel.id
                && p.broadcast_id == stream.broadcast_id
                && !p.task.is_finished()
            {
                return lock(&p.health).report();
            }
            *playing = None;
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        let mut viewer = Viewer {
            http: self.http.clone(),
            gql: Arc::clone(&self.gql),
            usher: self.usher.clone(),
            login: channel.login.clone(),
            media: None,
            requested: Requested::default(),
        };
        let first = viewer.poll().await;
        {
            let mut playing = lock(&self.playing);
            if self.generation.load(Ordering::SeqCst) == generation {
                let health = Arc::new(Mutex::new(Health::default()));
                let task = tokio::spawn(play(viewer, Arc::clone(&health), self.every));
                *playing = Some(Playing {
                    channel: channel.id.clone(),
                    broadcast_id: stream.broadcast_id.clone(),
                    health,
                    task,
                });
            }
        }

        match first? {
            0 => Err(nothing_served()),
            _ => Ok(()),
        }
    }

    /// Stops watching. Nothing is requested for the stream after this.
    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *lock(&self.playing) = None;
    }
}

async fn play(mut viewer: Viewer, health: Arc<Mutex<Health>>, every: Duration) {
    let mut next = Instant::now() + every;
    loop {
        tokio::time::sleep_until(next).await;
        next = Instant::now() + every;
        let result = viewer.poll().await;
        if let Err(e) = &result {
            tracing::debug!(channel = %viewer.login, error = %e, "playback poll failed");
        }
        lock(&health).record(result);
    }
}

/// What the background polls found between two heartbeats.
#[derive(Default)]
struct Health {
    served: usize,
    last_error: Option<TwitchError>,
}

impl Health {
    fn record(&mut self, result: Result<usize, TwitchError>) {
        match result {
            Ok(served) => self.served += served,
            Err(e) => self.last_error = Some(e),
        }
    }

    /// Video was served since the last report, or the reason it was not.
    fn report(&mut self) -> Result<(), TwitchError> {
        let served = std::mem::take(&mut self.served);
        let error = self.last_error.take();
        if served > 0 {
            Ok(())
        } else {
            Err(error.unwrap_or_else(nothing_served))
        }
    }
}

fn nothing_served() -> TwitchError {
    TwitchError::Refused("the stream's playlist listed no new video".into())
}

/// One stream being played: where its playlist is, and what has been asked for.
struct Viewer {
    http: reqwest::Client,
    gql: Arc<GqlClient>,
    usher: String,
    login: String,
    media: Option<(Url, Instant)>,
    requested: Requested,
}

impl Viewer {
    /// Reads the playlist and requests each segment not yet requested.
    /// Returns how many were served.
    async fn poll(&mut self) -> Result<usize, TwitchError> {
        let media = self.media_url().await?;
        let response = self
            .http
            .get(media.clone())
            .timeout(PLAYLIST_TIMEOUT)
            .send()
            .await
            .map_err(|e| TwitchError::Network(describe(&e)))?;
        let status = response.status().as_u16();
        if status != 200 {
            // A lapsed token, or a stream that has ended. Either way the next
            // poll starts again from the index rather than repeating this.
            if matches!(status, 401 | 403 | 404 | 410) {
                self.media = None;
            }
            return Err(TwitchError::Refused(format!(
                "Twitch answered HTTP {status} for the stream's playlist"
            )));
        }
        let playlist = response
            .text()
            .await
            .map_err(|e| TwitchError::Network(describe(&e)))?;

        let mut served = 0;
        let mut failure = None;
        for segment in segment_urls(&playlist, &media) {
            if self.requested.contains(&segment) {
                continue;
            }
            match self
                .http
                .head(segment.clone())
                .timeout(SEGMENT_TIMEOUT)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    served += 1;
                    self.requested.insert(segment);
                }
                Ok(r) => {
                    failure = Some(TwitchError::Refused(format!(
                        "Twitch answered HTTP {} for a video segment",
                        r.status().as_u16()
                    )));
                }
                Err(e) => failure = Some(TwitchError::Network(describe(&e))),
            }
        }
        match (served, failure) {
            (0, Some(why)) => Err(why),
            _ => Ok(served),
        }
    }

    /// The media playlist to follow, resolved afresh when there is none yet
    /// or the token behind it is getting old.
    async fn media_url(&mut self) -> Result<Url, TwitchError> {
        if let Some((url, resolved)) = &self.media
            && resolved.elapsed() < RENEW_AFTER
        {
            return Ok(url.clone());
        }

        let data = self
            .gql
            .run(
                &ops::PLAYBACK_ACCESS_TOKEN,
                json!({
                    "isLive": true,
                    "isVod": false,
                    "login": self.login,
                    "platform": "web",
                    "playerType": "site",
                    "vodID": "",
                }),
            )
            .await?;
        let (value, signature) = parse::parse_playback_token(&data)?;

        let mut index = Url::parse(&format!(
            "{}/api/channel/hls/{}.m3u8",
            self.usher.trim_end_matches('/'),
            self.login
        ))
        .map_err(|e| TwitchError::Malformed(format!("playlist index URL: {e}")))?;
        index
            .query_pairs_mut()
            .append_pair("sig", &signature)
            .append_pair("token", &value);

        let response = self
            .http
            .get(index.clone())
            .timeout(PLAYLIST_TIMEOUT)
            .send()
            .await
            .map_err(|e| TwitchError::Network(describe(&e)))?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(TwitchError::Refused(format!(
                "Twitch answered HTTP {status} for the stream's playlist index"
            )));
        }
        let body = response
            .text()
            .await
            .map_err(|e| TwitchError::Network(describe(&e)))?;
        let url = variant_url(&body, &index).ok_or_else(|| {
            TwitchError::Malformed("the stream's playlist index listed nothing to play".into())
        })?;
        tracing::debug!(channel = %self.login, "playback resolved");
        self.media = Some((url.clone(), Instant::now()));
        Ok(url)
    }
}

/// The segments already requested, oldest forgotten first.
#[derive(Default)]
struct Requested {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl Requested {
    fn contains(&self, url: &Url) -> bool {
        self.set.contains(url.as_str())
    }

    fn insert(&mut self, url: Url) {
        let key = String::from(url);
        if self.set.insert(key.clone()) {
            self.order.push_back(key);
        }
        while self.order.len() > REMEMBERED_SEGMENTS {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The variant to follow from a master playlist: the cheapest video.
///
/// Nothing is downloaded, so the choice costs nothing either way; it is made
/// by bandwidth rather than position because Twitch does not list variants
/// in quality order. A variant with a resolution is video; one without is
/// audio-only, and nobody has shown that listening earns drop time, so it is
/// followed only when nothing else is listed.
pub fn variant_url(master: &str, base: &Url) -> Option<Url> {
    let variants = uris_after(master, base, "#EXT-X-STREAM-INF:");
    let has_video = variants.iter().any(|(tag, _)| tag.contains("RESOLUTION="));
    variants
        .into_iter()
        .filter(|(tag, _)| !has_video || tag.contains("RESOLUTION="))
        .min_by_key(|(tag, _)| bandwidth(tag).unwrap_or(u64::MAX))
        .map(|(_, url)| url)
}

/// The video segments a media playlist lists, oldest first.
pub fn segment_urls(media: &str, base: &Url) -> Vec<Url> {
    uris_after(media, base, "#EXTINF:")
        .into_iter()
        .map(|(_, url)| url)
        .collect()
}

/// Each URI line that directly follows a `tag` line, with that tag.
///
/// URLs inside tags — an init segment, a trigger, a prefetch hint — are not
/// URI lines and are never returned. A body that is not a playlist at all,
/// such as the JSON error an offline channel gets, yields nothing.
fn uris_after<'a>(playlist: &'a str, base: &Url, tag: &str) -> Vec<(&'a str, Url)> {
    if !playlist.trim_start().starts_with("#EXTM3U") {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut pending: Option<&str> = None;
    for line in playlist.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.starts_with('#') {
            if line.starts_with(tag) {
                pending = Some(line);
            }
            continue;
        }
        if let Some(tag_line) = pending.take()
            && let Ok(url) = base.join(line)
            && matches!(url.scheme(), "http" | "https")
        {
            out.push((tag_line, url));
        }
    }
    out
}

fn bandwidth(tag: &str) -> Option<u64> {
    let start = tag.find("BANDWIDTH=")? + "BANDWIDTH=".len();
    let digits: String = tag[start..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> reqwest::Url {
        reqwest::Url::parse("https://edge.example/v1/playlist/abc.m3u8?token=t").unwrap()
    }

    /// Shaped on a real master playlist from 2026-10-10. Twitch does not list
    /// variants in quality order: here the last is 720p60, so "take the last
    /// line" — what the reference implementation does — picks a high one.
    const MASTER: &str = r#"#EXTM3U
#EXT-X-TWITCH-INFO:NODE="video-edge",SERVER-TIME="1791618000.00"
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="480p30",NAME="480p",AUTOSELECT=YES,DEFAULT=YES
#EXT-X-STREAM-INF:BANDWIDTH=1327200,RESOLUTION=852x480,CODECS="avc1.4D401F,mp4a.40.2",VIDEO="480p30",FRAME-RATE=30.000
https://a.playlist.example/v1/playlist/480.m3u8
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="160p30",NAME="160p",AUTOSELECT=YES,DEFAULT=YES
#EXT-X-STREAM-INF:BANDWIDTH=216299,RESOLUTION=284x160,CODECS="avc1.4D401F,mp4a.40.2",VIDEO="160p30",FRAME-RATE=30.000
https://a.playlist.example/v1/playlist/160.m3u8
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="720p60",NAME="720p60",AUTOSELECT=YES,DEFAULT=YES
#EXT-X-STREAM-INF:BANDWIDTH=3322199,RESOLUTION=1280x720,CODECS="avc1.4D401F,mp4a.40.2",VIDEO="720p60",FRAME-RATE=60.000
https://a.playlist.example/v1/playlist/720.m3u8
"#;

    /// Shaped on a real media playlist. Tags carry URLs of their own — the
    /// init segment, a trigger, a prefetch hint — and none of them is video
    /// the account has been served.
    const MEDIA: &str = r#"#EXTM3U
#EXT-X-VERSION:6
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:5816
#EXT-X-DATERANGE:ID="t",CLASS="twitch-trigger",START-DATE="2026-10-10T07:50:27Z",X-TV-TWITCH-TRIGGER-URL="https://a.playlist.example/trigger"
#EXT-X-MAP:URI="https://cdn.example/init.mp4"
#EXT-X-PROGRAM-DATE-TIME:2026-10-10T07:50:27.674Z
#EXTINF:2.000,live
https://cdn.example/seg/5816.mp4
#EXT-X-PROGRAM-DATE-TIME:2026-10-10T07:50:29.674Z
#EXTINF:2.000,live
https://cdn.example/seg/5817.mp4
#EXT-X-TWITCH-PREFETCH:https://cdn.example/seg/5818.mp4
"#;

    #[test]
    fn the_cheapest_variant_is_chosen_wherever_it_is_listed() {
        assert_eq!(
            variant_url(MASTER, &base()).map(|u| u.to_string()).as_deref(),
            Some("https://a.playlist.example/v1/playlist/160.m3u8")
        );
    }

    #[test]
    fn only_segments_count_not_urls_inside_tags() {
        let got: Vec<String> = segment_urls(MEDIA, &base())
            .into_iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            got,
            vec![
                "https://cdn.example/seg/5816.mp4".to_string(),
                "https://cdn.example/seg/5817.mp4".to_string(),
            ]
        );
    }

    #[test]
    fn relative_segments_resolve_against_the_playlist() {
        let media = "#EXTM3U\n#EXTINF:2.000,live\nseg/1.ts\n";
        assert_eq!(
            segment_urls(media, &base())
                .into_iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>(),
            vec!["https://edge.example/v1/playlist/seg/1.ts".to_string()]
        );
    }

    /// An offline channel's usher answer is a JSON error, not a playlist.
    /// Reading a URL out of it would send the next request somewhere absurd.
    #[test]
    fn an_error_body_is_not_a_playlist() {
        let body = r#"[{"url":"https://usher.example/x","error":"Can not find channel","error_code":"vod_manifest_restricted"}]"#;
        assert!(variant_url(body, &base()).is_none());
        assert!(segment_urls(body, &base()).is_empty());
    }

    /// Twitch can list an audio-only rendition, and it is always the cheapest.
    /// Nobody has shown that listening earns drop time, so following it could
    /// make a heartbeat report success while earning nothing.
    #[test]
    fn audio_only_is_not_chosen_while_there_is_video() {
        let master = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360,VIDEO=\"360p30\"\n\
            https://a.example/360.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=160000,CODECS=\"mp4a.40.2\",VIDEO=\"audio_only\"\n\
            https://a.example/audio.m3u8\n";
        assert_eq!(
            variant_url(master, &base()).map(|u| u.to_string()).as_deref(),
            Some("https://a.example/360.m3u8")
        );
    }

    /// A variant listed without a bandwidth is still a variant: it is used
    /// rather than reporting the stream unwatchable.
    #[test]
    fn a_master_without_bandwidths_still_yields_a_variant() {
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=640x360\nhttps://a.example/only.m3u8\n";
        assert_eq!(
            variant_url(master, &base()).map(|u| u.to_string()).as_deref(),
            Some("https://a.example/only.m3u8")
        );
    }
}
