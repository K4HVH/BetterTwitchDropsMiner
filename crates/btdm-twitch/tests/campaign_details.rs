//! Campaign details, against a real socket.
//!
//! Observed live on 2026-10-24: the persisted `DropCampaignDetails` query
//! answered `"user": null` when `channelLogin` held the account's login, and
//! the campaign when it held the account's user id. Every linked campaign the
//! dashboard offered came back empty that way. Every sync the event log still
//! held, back to 2026-10-16, reported 0 mineable; on the day it was checked,
//! Twitch had thirty-two campaigns open to the account.

use btdm_core::api::TwitchApi;
use btdm_core::model::{CampaignId, UserId};
use btdm_twitch::api_impl::TwitchClient;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const USER_ID: &str = "530826710";
const LOGIN: &str = "k4hvh";

fn campaign(id: &str) -> Value {
    json!({
        "id": id,
        "name": "Camp",
        "startAt": "2026-10-01T00:00:00Z",
        "endAt": "2099-01-01T00:00:00Z",
        "game": { "id": "g1", "name": "Game", "slug": "game" },
        "self": { "isAccountConnected": true },
        "timeBasedDrops": [{
            "id": "d1",
            "name": "d1",
            "requiredMinutesWatched": 60,
            "benefitEdges": [{ "benefit": { "id": "b1", "name": "b1" } }],
        }],
    })
}

/// One operation's answer.
///
/// The persisted query looks the account up by id, as Twitch's did. A raw
/// query is answered by whichever lookup its text actually asks for, so a
/// fallback that sends an id into `user(login:)` finds nobody, as it would
/// against the real thing.
fn answer(op: &Value, stale_hash: bool) -> Value {
    let raw = op.get("query").and_then(Value::as_str);
    if raw.is_none() && stale_hash {
        return json!({ "errors": [{ "message": "PersistedQueryNotFound" }] });
    }
    let by_id = raw.is_none_or(|q| q.contains("user(id: $channelLogin)"));
    let asked = op["variables"]["channelLogin"].as_str().unwrap_or("");
    let found = if by_id { asked == USER_ID } else { asked == LOGIN };
    if !found {
        return json!({ "data": { "user": null } });
    }
    let id = op["variables"]["dropID"].as_str().unwrap();
    json!({ "data": { "user": { "dropCampaign": campaign(id) } } })
}

async fn read_body(socket: &mut tokio::net::TcpStream) -> Option<String> {
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
            return Some(text[split + 4..].to_owned());
        }
    }
}

/// A Twitch answering campaign details, single or batched. Returns its URL.
async fn fake_twitch(stale_hash: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let Some(body) = read_body(&mut socket).await else {
                    return;
                };
                let request: Value = serde_json::from_str(&body).unwrap();
                let reply = match &request {
                    Value::Array(ops) => {
                        Value::Array(ops.iter().map(|op| answer(op, stale_hash)).collect())
                    }
                    op => answer(op, stale_hash),
                }
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    format!("http://{addr}/gql")
}

fn client(endpoint: String) -> TwitchClient {
    TwitchClient::with_endpoint(
        reqwest::Client::new(),
        "token".into(),
        UserId::from(USER_ID),
        endpoint,
    )
}

#[tokio::test]
async fn a_linked_campaign_is_described() {
    let api = client(fake_twitch(false).await);
    let answers = api.campaign_details_many(&[CampaignId::from("c1")]).await;

    assert_eq!(answers.len(), 1);
    let described = answers[0].1.as_ref().expect("the request itself succeeded");
    assert_eq!(
        described.as_ref().map(|c| c.id.0.as_str()),
        Some("c1"),
        "Twitch describes this campaign to the account that asks by id"
    );
}

/// Hashes rot, and the raw query is what keeps the miner working the day one
/// does. It has to ask for the account the same way the variable names it.
#[tokio::test]
async fn the_raw_fallback_finds_the_account_too() {
    let api = client(fake_twitch(true).await);
    let answers = api.campaign_details_many(&[CampaignId::from("c1")]).await;

    let described = answers[0].1.as_ref().expect("the fallback itself succeeded");
    assert!(described.is_some(), "the raw query found nobody to ask about");
}
