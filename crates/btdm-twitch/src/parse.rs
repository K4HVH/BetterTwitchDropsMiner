//! Twitch JSON to domain model.
//!
//! Kept separate from transport so fixtures can exercise it without a network,
//! and so a schema change shows up as a failing parse test rather than a
//! mysterious runtime behaviour.

use crate::TwitchError;
use btdm_core::api::DropProgress;
use btdm_core::model::*;
use chrono::{DateTime, Utc};
use serde_json::Value;

fn malformed(what: &str) -> TwitchError {
    TwitchError::Malformed(what.to_owned())
}

fn str_at<'a>(v: &'a Value, key: &str) -> Result<&'a str, TwitchError> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(&format!("missing string `{key}`")))
}

fn time_at(v: &Value, key: &str) -> Result<DateTime<Utc>, TwitchError> {
    let raw = str_at(v, key)?;
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| malformed(&format!("`{key}` is not RFC3339: {raw}")))
}

pub fn parse_benefit_kind(raw: &str) -> BenefitKind {
    match raw {
        "BADGE" => BenefitKind::Badge,
        "EMOTE" => BenefitKind::Emote,
        _ => BenefitKind::Entitlement,
    }
}

fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

/// Twitch is inconsistent about how it names a game across queries: the
/// inventory returns `name` and `slug`, while the campaign dashboard returns
/// `displayName` and no slug at all. Requiring `name` silently discarded 129
/// of 130 campaigns against a real account, so both spellings are accepted and
/// the slug is derived when absent.
fn parse_game(v: &Value) -> Result<Game, TwitchError> {
    let name = v
        .get("name")
        .or_else(|| v.get("displayName"))
        .and_then(Value::as_str)
        .ok_or_else(|| malformed("game has neither `name` nor `displayName`"))?
        .to_owned();

    Ok(Game {
        id: GameId(str_at(v, "id")?.to_owned()),
        slug: v
            .get("slug")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| slugify(&name)),
        name,
    })
}

fn parse_channel(v: &Value) -> Result<Channel, TwitchError> {
    // Directory nodes use `login`; campaign allow-lists use `name`.
    let login = v
        .get("login")
        .or_else(|| v.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| malformed("channel has no login/name"))?;
    Ok(Channel {
        id: ChannelId(str_at(v, "id")?.to_owned()),
        login: login.to_owned(),
        display_name: v
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or(login)
            .to_owned(),
    })
}

fn parse_drop(v: &Value) -> Result<TimedDrop, TwitchError> {
    let selfv = v.get("self").filter(|s| !s.is_null());
    let benefits = v
        .get("benefitEdges")
        .and_then(Value::as_array)
        .map(|edges| {
            edges
                .iter()
                .filter_map(|e| e.get("benefit"))
                .filter_map(|b| {
                    Some(Benefit {
                        id: b.get("id")?.as_str()?.to_owned(),
                        name: b.get("name")?.as_str()?.to_owned(),
                        kind: parse_benefit_kind(
                            b.get("distributionType")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        ),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(TimedDrop {
        id: DropId(str_at(v, "id")?.to_owned()),
        name: v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        benefits,
        required_minutes: v
            .get("requiredMinutesWatched")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        current_minutes: selfv
            .and_then(|s| s.get("currentMinutesWatched"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        claim_id: selfv
            .and_then(|s| s.get("dropInstanceID"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        is_claimed: selfv
            .and_then(|s| s.get("isClaimed"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        precondition_ids: v
            .get("preconditionDrops")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.get("id").and_then(Value::as_str))
                    .map(|s| DropId(s.to_owned()))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

pub fn parse_campaign(v: &Value) -> Result<DropCampaign, TwitchError> {
    let game = parse_game(
        v.get("game")
            .filter(|g| !g.is_null())
            .ok_or_else(|| malformed("campaign has no game"))?,
    )?;
    let drops = v
        .get("timeBasedDrops")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_drop).collect::<Result<Vec<_>, _>>())
        .transpose()?
        .unwrap_or_default();
    let allowed_channels = v
        .get("allow")
        .filter(|a| !a.is_null())
        .and_then(|a| a.get("channels"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|c| parse_channel(c).ok()).collect())
        .unwrap_or_default();

    Ok(DropCampaign {
        id: CampaignId(str_at(v, "id")?.to_owned()),
        name: v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        game,
        starts_at: time_at(v, "startAt")?,
        ends_at: time_at(v, "endAt")?,
        drops,
        // Absent `self` means Twitch did not confirm a link, so treat it as
        // unlinked. Guessing optimistically here is what produces a miner that
        // grinds at campaigns it can never progress.
        is_account_linked: v
            .get("self")
            .and_then(|s| s.get("isAccountConnected"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        allowed_channels,
    })
}

/// Campaigns whose shape is broken are skipped individually rather than
/// failing the whole sync — one malformed campaign must not blind the miner
/// to the other forty.
fn campaign_list(data: &Value, path: &[&str]) -> Result<Vec<DropCampaign>, TwitchError> {
    let mut node = data;
    for key in path {
        node = node
            .get(key)
            .filter(|v| !v.is_null())
            .ok_or_else(|| malformed(&format!("missing `{key}`")))?;
    }
    let array = node
        .as_array()
        .ok_or_else(|| malformed("expected an array of campaigns"))?;

    let mut out = Vec::with_capacity(array.len());
    for raw in array {
        match parse_campaign(raw) {
            Ok(c) => out.push(c),
            Err(e) => tracing::warn!(error = %e, "skipping unparseable campaign"),
        }
    }
    Ok(out)
}

pub fn parse_inventory(data: &Value) -> Result<Vec<DropCampaign>, TwitchError> {
    campaign_list(data, &["currentUser", "inventory", "dropCampaignsInProgress"])
}

pub fn parse_campaigns(data: &Value) -> Result<Vec<DropCampaign>, TwitchError> {
    campaign_list(data, &["currentUser", "dropCampaigns"])
}

pub fn parse_campaign_details(data: &Value) -> Result<Option<DropCampaign>, TwitchError> {
    let Some(node) = data
        .get("user")
        .filter(|v| !v.is_null())
        .and_then(|u| u.get("dropCampaign"))
        .filter(|v| !v.is_null())
    else {
        return Ok(None);
    };
    parse_campaign(node).map(Some)
}

pub fn parse_directory(data: &Value) -> Result<Vec<(Channel, Stream)>, TwitchError> {
    let edges = data
        .get("game")
        .filter(|v| !v.is_null())
        .and_then(|g| g.get("streams"))
        .filter(|v| !v.is_null())
        .and_then(|s| s.get("edges"))
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("directory has no stream edges"))?;

    let mut out = Vec::new();
    for edge in edges {
        let Some(node) = edge.get("node").filter(|n| !n.is_null()) else {
            continue;
        };
        let Some(bc) = node.get("broadcaster").filter(|b| !b.is_null()) else {
            continue;
        };
        let Ok(channel) = parse_channel(bc) else {
            continue;
        };
        let game = node.get("game").and_then(|g| parse_game(g).ok());
        let drops_enabled = node
            .get("freeformTags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter().any(|t| {
                    t.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.replace(' ', "").eq_ignore_ascii_case("DropsEnabled"))
                })
            })
            .unwrap_or(false);

        out.push((
            channel,
            Stream {
                broadcast_id: node
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                game,
                title: node
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                viewers: node.get("viewersCount").and_then(Value::as_u64).unwrap_or(0) as u32,
                drops_enabled,
            },
        ));
    }
    Ok(out)
}

/// `Ok(None)` when the channel is offline.
pub fn parse_stream_info(data: &Value) -> Result<Option<(Channel, Stream)>, TwitchError> {
    let Some(user) = data.get("user").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let channel = parse_channel(user)?;
    let Some(stream) = user.get("stream").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let settings = user.get("broadcastSettings").filter(|s| !s.is_null());

    Ok(Some((
        channel,
        Stream {
            broadcast_id: stream
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            game: settings
                .and_then(|s| s.get("game"))
                .filter(|g| !g.is_null())
                .and_then(|g| parse_game(g).ok()),
            title: settings
                .and_then(|s| s.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            viewers: stream
                .get("viewersCount")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            // This shape carries no tags; eligibility is verified separately
            // against the channel's actually-available drops.
            drops_enabled: true,
        },
    )))
}

/// Drop IDs the channel can currently progress for this account.
pub fn parse_available_drops(data: &Value) -> Vec<DropId> {
    let mut ids = Vec::new();
    if let Some(campaigns) = data
        .get("channel")
        .filter(|c| !c.is_null())
        .and_then(|c| c.get("viewerDropCampaigns"))
        .and_then(Value::as_array)
    {
        for campaign in campaigns {
            let Some(drops) = campaign.get("timeBasedDrops").and_then(Value::as_array) else {
                continue;
            };
            ids.extend(
                drops
                    .iter()
                    .filter_map(|d| d.get("id").and_then(Value::as_str))
                    .map(|s| DropId(s.to_owned())),
            );
        }
    }
    ids
}

/// Returns the channel the session belongs to alongside the progress.
///
/// The channel matters: Twitch reports a single drop session per *account*,
/// so if the user is also watching in a browser this query returns the
/// browser's progress. Without the channel the miner cannot tell that apart
/// from its own, and an unrelated session would keep resetting its stall clock
/// while it earned nothing.
pub fn parse_current_progress(data: &Value) -> Option<(Option<ChannelId>, DropProgress)> {
    let session = data
        .get("currentUser")
        .filter(|v| !v.is_null())
        .and_then(|u| u.get("dropCurrentSession"))
        .filter(|v| !v.is_null())?;

    let channel = session
        .get("channel")
        .filter(|c| !c.is_null())
        .and_then(|c| c.get("id"))
        .and_then(Value::as_str)
        .map(|s| ChannelId(s.to_owned()));

    Some((
        channel,
        DropProgress {
            drop_id: DropId(session.get("dropID")?.as_str()?.to_owned()),
            current_minutes: session
                .get("currentMinutesWatched")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            required_minutes: session
                .get("requiredMinutesWatched")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        },
    ))
}
