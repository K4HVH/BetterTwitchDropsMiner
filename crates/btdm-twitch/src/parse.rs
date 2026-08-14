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
        box_art_url: v
            .get("boxArtURL")
            .and_then(Value::as_str)
            .map(size_template),
        name,
    })
}

/// Twitch returns image URLs as templates like
/// `.../ttv-boxart/12345-{width}x{height}.jpg`. Handing that to an `<img>`
/// tag renders a broken image, so the placeholders are filled in.
fn size_template(url: &str) -> String {
    url.replace("{width}", "144").replace("{height}", "192")
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
                        image_url: b
                            .get("imageAssetURL")
                            .and_then(Value::as_str)
                            .map(size_template),
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
        // Twitch only sometimes narrows a tier's window inside its campaign's.
        starts_at: v
            .get("startAt")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc)),
        ends_at: v
            .get("endAt")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc)),
        // Reward campaigns come down the same pipe as watch-time ones, and
        // are distinguishable only by this field.
        required_subs: v
            .get("requiredSubs")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
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
    let mut drops = v
        .get("timeBasedDrops")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_drop).collect::<Result<Vec<_>, _>>())
        .transpose()?
        .unwrap_or_default();
    order_tiers(&mut drops);
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
        account_link_url: v
            .get("accountLinkURL")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_owned),
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
    let mut campaigns =
        campaign_list(data, &["currentUser", "inventory", "dropCampaignsInProgress"])?;

    // Cross-reference the claimed-benefit ledger.
    //
    // `gameEventDrops` was requested and thrown away. It matters because a
    // campaign whose detail response carries no `self` edge parses as
    // "unclaimed, zero minutes", so an already-claimed drop is mined all over
    // again. If every one of a drop's benefits was awarded inside that drop's
    // own window, it has been claimed.
    let awarded = claimed_benefits(data);
    if !awarded.is_empty() {
        for campaign in &mut campaigns {
            let window = (campaign.starts_at, campaign.ends_at);
            for drop in &mut campaign.drops {
                if drop.is_claimed || drop.benefits.is_empty() {
                    continue;
                }
                let from = drop.starts_at.unwrap_or(window.0);
                let until = drop.ends_at.unwrap_or(window.1);
                let all_awarded = drop.benefits.iter().all(|b| {
                    awarded
                        .get(&b.id)
                        .is_some_and(|at| *at >= from && *at < until)
                });
                if all_awarded {
                    tracing::debug!(
                        drop = %drop.id,
                        "inferred claimed from the awarded-benefit ledger"
                    );
                    drop.is_claimed = true;
                }
            }
        }
    }
    Ok(campaigns)
}

/// Benefit id to when it was last awarded.
fn claimed_benefits(data: &Value) -> std::collections::HashMap<String, DateTime<Utc>> {
    let mut out = std::collections::HashMap::new();
    let Some(list) = data
        .get("currentUser")
        .and_then(|u| u.get("inventory"))
        .and_then(|i| i.get("gameEventDrops"))
        .and_then(Value::as_array)
    else {
        return out;
    };

    for entry in list {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(at) = entry
            .get("lastAwardedAt")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        else {
            continue;
        };
        out.insert(id.to_owned(), at.with_timezone(&Utc));
    }
    out
}

pub fn parse_campaigns(data: &Value) -> Result<Vec<DropCampaign>, TwitchError> {
    campaign_list(data, &["currentUser", "dropCampaigns"])
}

/// Puts a campaign's tiers into the order a reader expects.
///
/// Twitch returns them in no particular order — a real campaign came back as
/// 240, 15, 30, 60, 120, 180 minutes, and since every tier of it was also
/// named "Madden Twitch Pack" the list read as six identical rewards at
/// random thresholds.
///
/// Sorting on required minutes alone is not enough, because Twitch expresses
/// tiers two different ways. Most campaigns give each tier a *cumulative*
/// requirement, where minutes ascending is exactly the tier order. Others gate
/// a tier behind earlier ones and give it its own smaller requirement — a
/// 5-minute finale behind a 10-minute and a 100-minute tier — and there,
/// sorting by minutes puts the finale first, above the two tiers it depends
/// on. So preconditions come first: a tier never appears before something it
/// is gated behind.
///
/// Subscription rewards need zero minutes and would otherwise head every
/// mixed campaign, putting the one reward the account can never earn by
/// watching at the top of the card.
/// Where one tier sorts relative to the others.
///
/// Depth first, so nothing appears before something it is gated behind. Then
/// watch-time tiers ahead of subscription rewards, which need zero minutes and
/// would otherwise head every mixed campaign. Then minutes, which is the tier
/// order for the ordinary cumulative shape.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct TierOrder {
    depth: usize,
    needs_subscription: bool,
    required_minutes: u32,
    name: String,
    id: String,
}

fn order_tiers(drops: &mut Vec<TimedDrop>) {
    // How many tiers deep in a precondition chain this one sits. Bounded by
    // the number of drops, so a cycle in Twitch's data cannot spin here.
    fn depth(drops: &[TimedDrop], drop: &TimedDrop, budget: usize) -> usize {
        if budget == 0 {
            return 0;
        }
        drop.precondition_ids
            .iter()
            .filter_map(|id| drops.iter().find(|d| &d.id == id))
            .map(|parent| 1 + depth(drops, parent, budget - 1))
            .max()
            .unwrap_or(0)
    }

    let keys: Vec<TierOrder> = drops
        .iter()
        .map(|d| TierOrder {
            depth: depth(drops, d, drops.len()),
            needs_subscription: d.required_subs > 0,
            required_minutes: d.required_minutes,
            name: d.name.clone(),
            id: d.id.0.clone(),
        })
        .collect();

    let mut paired: Vec<(TierOrder, TimedDrop)> =
        keys.into_iter().zip(drops.drain(..)).collect();
    paired.sort_by(|a, b| a.0.cmp(&b.0));
    drops.extend(paired.into_iter().map(|(_, d)| d));
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

/// Whether Twitch actually granted a claim.
///
/// `ELIGIBLE_FOR_ALL` is the success path; `DROP_INSTANCE_ALREADY_CLAIMED`
/// means the reward is already on the account, which is equally fine. Anything
/// else — including a missing status — has to be treated as a failure, because
/// marking a drop claimed locally means it is never retried.
pub fn claim_granted(response: &Value) -> Result<(), String> {
    let status = response
        .get("claimDropRewards")
        .filter(|v| !v.is_null())
        .and_then(|v| v.get("status"))
        .and_then(Value::as_str);

    match status {
        Some("ELIGIBLE_FOR_ALL") | Some("DROP_INSTANCE_ALREADY_CLAIMED") => Ok(()),
        Some(other) => Err(format!("Twitch refused the claim: {other}")),
        None => Err("the claim response carried no status".to_owned()),
    }
}

#[cfg(test)]
mod claim_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_granted_claim_is_accepted() {
        assert!(claim_granted(&json!({"claimDropRewards": {"status": "ELIGIBLE_FOR_ALL"}})).is_ok());
    }

    /// Already claimed means the reward is on the account, which is what the
    /// caller wanted.
    #[test]
    fn an_already_claimed_drop_is_not_an_error() {
        assert!(
            claim_granted(&json!({
                "claimDropRewards": {"status": "DROP_INSTANCE_ALREADY_CLAIMED"}
            }))
            .is_ok()
        );
    }

    /// The bug this exists to prevent: a refused claim reported as a success,
    /// the drop marked claimed locally, and never retried.
    #[test]
    fn a_refused_claim_is_a_failure() {
        let err = claim_granted(&json!({"claimDropRewards": {"status": "INELIGIBLE"}}))
            .unwrap_err();
        assert!(err.contains("INELIGIBLE"), "{err}");
    }

    #[test]
    fn a_null_or_missing_payload_is_a_failure() {
        assert!(claim_granted(&json!({"claimDropRewards": null})).is_err());
        assert!(claim_granted(&json!({})).is_err());
        assert!(claim_granted(&json!({"claimDropRewards": {}})).is_err());
    }
}

#[cfg(test)]
mod ledger_tests {
    use super::*;
    use serde_json::json;

    fn inventory(drops: Value, awarded: Value) -> Value {
        json!({
            "currentUser": {
                "inventory": {
                    "dropCampaignsInProgress": [{
                        "id": "c1",
                        "name": "Camp",
                        "startAt": "2026-08-01T00:00:00Z",
                        "endAt": "2026-09-01T00:00:00Z",
                        "game": { "id": "g1", "name": "Game", "slug": "game" },
                        "self": { "isAccountConnected": true },
                        "timeBasedDrops": drops,
                    }],
                    "gameEventDrops": awarded,
                }
            }
        })
    }

    fn tier(id: &str, benefit: &str) -> Value {
        json!({
            "id": id,
            "name": id,
            "requiredMinutesWatched": 60,
            "benefitEdges": [{ "benefit": { "id": benefit, "name": benefit } }],
        })
    }

    /// A campaign whose detail response carries no `self` edge parses as
    /// "unclaimed, zero minutes", so an already-claimed drop is mined all over
    /// again. The awarded-benefit ledger is how Twitch's own client knows
    /// better, and it was being fetched and discarded.
    #[test]
    fn a_drop_whose_benefits_were_all_awarded_is_treated_as_claimed() {
        let data = inventory(
            json!([tier("d1", "b1")]),
            json!([{ "id": "b1", "lastAwardedAt": "2026-08-05T12:00:00Z" }]),
        );
        let campaigns = parse_inventory(&data).unwrap();
        assert!(campaigns[0].drops[0].is_claimed);
    }

    /// Awarded *before* the campaign began is a different campaign's reward
    /// that happens to grant the same benefit.
    #[test]
    fn a_benefit_awarded_outside_the_window_proves_nothing() {
        let data = inventory(
            json!([tier("d1", "b1")]),
            json!([{ "id": "b1", "lastAwardedAt": "2020-01-01T00:00:00Z" }]),
        );
        let campaigns = parse_inventory(&data).unwrap();
        assert!(!campaigns[0].drops[0].is_claimed);
    }

    /// Every benefit must have been awarded, not just one of them.
    #[test]
    fn a_partially_awarded_drop_is_not_claimed() {
        let mut two = tier("d1", "b1");
        two["benefitEdges"] = json!([
            { "benefit": { "id": "b1", "name": "one" } },
            { "benefit": { "id": "b2", "name": "two" } },
        ]);
        let data = inventory(
            json!([two]),
            json!([{ "id": "b1", "lastAwardedAt": "2026-08-05T12:00:00Z" }]),
        );
        let campaigns = parse_inventory(&data).unwrap();
        assert!(!campaigns[0].drops[0].is_claimed);
    }

    #[test]
    fn an_absent_or_malformed_ledger_changes_nothing() {
        for ledger in [json!([]), json!(null), json!("nonsense"),
                       json!([{ "id": "b1" }, { "lastAwardedAt": "x" }])] {
            let data = inventory(json!([tier("d1", "b1")]), ledger);
            let campaigns = parse_inventory(&data).unwrap();
            assert!(!campaigns[0].drops[0].is_claimed);
        }
    }

    /// An explicit `self.isClaimed` from Twitch always wins over inference.
    #[test]
    fn the_servers_own_answer_is_not_overridden() {
        let mut t = tier("d1", "b1");
        t["self"] = json!({ "isClaimed": false, "currentMinutesWatched": 10 });
        let data = inventory(
            json!([t]),
            json!([{ "id": "b1", "lastAwardedAt": "2020-01-01T00:00:00Z" }]),
        );
        let campaigns = parse_inventory(&data).unwrap();
        assert!(!campaigns[0].drops[0].is_claimed);
        assert_eq!(campaigns[0].drops[0].current_minutes, 10);
    }

    #[test]
    fn a_per_drop_window_is_parsed_when_present() {
        let mut t = tier("d1", "b1");
        t["startAt"] = json!("2026-08-10T00:00:00Z");
        t["endAt"] = json!("2026-08-12T00:00:00Z");
        let data = inventory(json!([t]), json!([]));
        let campaigns = parse_inventory(&data).unwrap();
        assert!(campaigns[0].drops[0].starts_at.is_some());
        assert!(campaigns[0].drops[0].ends_at.is_some());
    }
}
