use btdm_core::model::*;
use btdm_twitch::parse::*;

fn fixture(name: &str) -> serde_json::Value {
    let raw = match name {
        "inventory" => include_str!("fixtures/inventory.json"),
        "directory" => include_str!("fixtures/directory.json"),
        other => panic!("unknown fixture {other}"),
    };
    serde_json::from_str(raw).expect("fixture is valid JSON")
}

fn inventory() -> Vec<DropCampaign> {
    parse_inventory(&fixture("inventory")).unwrap()
}

#[test]
fn parses_the_well_formed_campaigns_and_skips_the_broken_one() {
    // camp-3 has a null game and must be skipped without failing the whole sync.
    let inv = inventory();
    assert_eq!(inv.len(), 2);
    assert!(inv.iter().all(|c| c.id != CampaignId::from("camp-3")));
}

#[test]
fn parses_drop_progress_and_requirements() {
    let c = &inventory()[0];
    assert_eq!(c.id, CampaignId::from("camp-1"));
    assert_eq!(c.game.slug, "deadlock");
    assert_eq!(c.drops.len(), 2);
    assert_eq!(c.drops[0].required_minutes, 60);
    assert_eq!(c.drops[0].current_minutes, 23);
    assert_eq!(c.drops[0].remaining_minutes(), 37);
}

#[test]
fn a_drop_instance_id_becomes_a_claim_id() {
    let c = &inventory()[0];
    assert_eq!(c.drops[0].claim_id, None, "not yet earned");
    assert_eq!(c.drops[1].claim_id.as_deref(), Some("inst-2"));
    assert!(c.drops[1].is_claimable());
}

#[test]
fn benefit_kinds_are_mapped() {
    let inv = inventory();
    assert_eq!(inv[0].drops[0].benefits[0].kind, BenefitKind::Entitlement);
    assert_eq!(inv[0].drops[1].benefits[0].kind, BenefitKind::Emote);
    assert_eq!(inv[1].drops[0].benefits[0].kind, BenefitKind::Badge);
}

#[test]
fn precondition_tiers_are_parsed() {
    let c = &inventory()[0];
    assert_eq!(c.drops[1].precondition_ids, vec![DropId::from("drop-1")]);
    assert!(c.drops[0].precondition_ids.is_empty());
}

#[test]
fn account_link_state_is_taken_from_the_server_never_assumed() {
    let inv = inventory();
    assert!(inv[0].is_account_linked);
    assert!(!inv[1].is_account_linked, "server says unlinked");
}

/// A missing `self` block must read as unlinked. Guessing optimistically here
/// produces a miner that grinds at campaigns it can never progress.
#[test]
fn a_campaign_with_no_self_block_is_treated_as_unlinked() {
    let v = serde_json::json!({
        "id": "c9", "name": "No self",
        "startAt": "2026-08-01T00:00:00Z", "endAt": "2026-09-01T00:00:00Z",
        "game": { "id": "g", "name": "G", "slug": "g" },
        "timeBasedDrops": []
    });
    assert!(!parse_campaign(&v).unwrap().is_account_linked);
}

#[test]
fn allowed_channels_are_parsed() {
    let inv = inventory();
    assert_eq!(inv[0].allowed_channels.len(), 1);
    assert_eq!(inv[0].allowed_channels[0].login, "streamer1");
    assert!(inv[1].allowed_channels.is_empty(), "empty means any channel");
}

#[test]
fn a_missing_game_slug_is_derived_from_the_name() {
    let v = serde_json::json!({
        "id": "c9", "name": "X",
        "startAt": "2026-08-01T00:00:00Z", "endAt": "2026-09-01T00:00:00Z",
        "game": { "id": "g", "name": "Sea of Thieves" },
        "self": { "isAccountConnected": true },
        "timeBasedDrops": []
    });
    assert_eq!(parse_campaign(&v).unwrap().game.slug, "sea-of-thieves");
}

#[test]
fn malformed_input_errors_rather_than_panicking() {
    assert!(parse_inventory(&serde_json::json!({ "currentUser": null })).is_err());
    assert!(parse_inventory(&serde_json::json!({})).is_err());
}

#[test]
fn directory_parses_channels_and_their_drops_tag() {
    let channels = parse_directory(&fixture("directory")).unwrap();
    // The node with a null broadcaster is skipped rather than fatal.
    assert_eq!(channels.len(), 2);

    let (c1, s1) = &channels[0];
    assert_eq!(c1.login, "streamer1");
    assert_eq!(s1.broadcast_id, "bc-1");
    assert_eq!(s1.viewers, 1234);
    assert!(s1.drops_enabled, "DropsEnabled tag present");

    let (_, s2) = &channels[1];
    assert!(!s2.drops_enabled, "no DropsEnabled tag");
}

#[test]
fn stream_info_reports_an_offline_channel_as_none() {
    let offline = serde_json::json!({
        "user": { "id": "1", "login": "s", "displayName": "S", "stream": null }
    });
    assert!(parse_stream_info(&offline).unwrap().is_none());

    let no_user = serde_json::json!({ "user": null });
    assert!(parse_stream_info(&no_user).unwrap().is_none());
}

#[test]
fn stream_info_parses_a_live_channel() {
    let live = serde_json::json!({
        "user": {
            "id": "1", "login": "s", "displayName": "S",
            "stream": { "id": "bc-7", "viewersCount": 42 },
            "broadcastSettings": { "title": "hello", "game": { "id": "g", "name": "Deadlock", "slug": "deadlock" } }
        }
    });
    let (channel, stream) = parse_stream_info(&live).unwrap().unwrap();
    assert_eq!(channel.login, "s");
    assert_eq!(stream.broadcast_id, "bc-7");
    assert_eq!(stream.game.unwrap().name, "Deadlock");
}

#[test]
fn available_drops_flattens_every_campaign() {
    let v = serde_json::json!({
        "channel": { "viewerDropCampaigns": [
            { "id": "c1", "timeBasedDrops": [{ "id": "d1" }, { "id": "d2" }] },
            { "id": "c2", "timeBasedDrops": [{ "id": "d3" }] }
        ]}
    });
    assert_eq!(
        parse_available_drops(&v),
        vec![
            DropId::from("d1"),
            DropId::from("d2"),
            DropId::from("d3")
        ]
    );
}

#[test]
fn available_drops_is_empty_when_the_channel_progresses_nothing() {
    let empty = serde_json::json!({ "channel": { "viewerDropCampaigns": [] } });
    assert!(parse_available_drops(&empty).is_empty());

    let null_channel = serde_json::json!({ "channel": null });
    assert!(parse_available_drops(&null_channel).is_empty());
}

#[test]
fn current_progress_parses_an_active_session() {
    let v = serde_json::json!({
        "currentUser": { "dropCurrentSession": {
            "channel": { "id": "ch-1" },
            "game": { "id": "g", "name": "Deadlock", "slug": "deadlock" },
            "currentMinutesWatched": 23,
            "requiredMinutesWatched": 60,
            "dropID": "drop-1"
        }}
    });
    let (channel, p) = parse_current_progress(&v).unwrap();
    assert_eq!(
        channel,
        Some(ChannelId::from("ch-1")),
        "the session's channel is what lets the caller reject another session's progress"
    );
    assert_eq!(p.drop_id, DropId::from("drop-1"));
    assert_eq!(p.current_minutes, 23);
    assert_eq!(p.required_minutes, 60);
}

#[test]
fn current_progress_is_none_when_there_is_no_session() {
    assert!(
        parse_current_progress(&serde_json::json!({
            "currentUser": { "dropCurrentSession": null }
        }))
        .is_none()
    );
}

#[test]
fn campaign_details_unwraps_the_nested_shape() {
    let v = serde_json::json!({
        "user": { "dropCampaign": {
            "id": "c1", "name": "Camp",
            "startAt": "2026-08-01T00:00:00Z", "endAt": "2026-09-01T00:00:00Z",
            "game": { "id": "g", "name": "Deadlock", "slug": "deadlock" },
            "self": { "isAccountConnected": true },
            "timeBasedDrops": [{
                "id": "d1", "name": "R", "requiredMinutesWatched": 60,
                "benefitEdges": [], "self": { "currentMinutesWatched": 0, "isClaimed": false }
            }]
        }}
    });
    let c = parse_campaign_details(&v).unwrap().unwrap();
    assert_eq!(c.id, CampaignId::from("c1"));
    assert_eq!(c.drops.len(), 1);

    assert!(
        parse_campaign_details(&serde_json::json!({ "user": null }))
            .unwrap()
            .is_none()
    );
}

/// The campaign dashboard names games with `displayName` and omits `slug`,
/// while the inventory uses `name` and `slug`. Requiring `name` silently
/// discarded 129 of 130 campaigns against a real account.
#[test]
fn a_game_named_with_display_name_still_parses() {
    let v = serde_json::json!({
        "id": "c1", "name": "Official 13",
        "startAt": "2026-08-03T13:00:00Z", "endAt": "2026-08-10T12:59:59.999Z",
        "game": { "id": "2099590881", "displayName": "Out of the Park Baseball 27" },
        "self": { "isAccountConnected": true },
        "timeBasedDrops": []
    });
    let c = parse_campaign(&v).unwrap();
    assert_eq!(c.game.name, "Out of the Park Baseball 27");
    assert_eq!(
        c.game.slug, "out-of-the-park-baseball-27",
        "the directory lookup needs a slug even when Twitch omits one"
    );
}

#[test]
fn a_game_with_neither_name_nor_display_name_is_rejected() {
    let v = serde_json::json!({
        "id": "c1", "name": "X",
        "startAt": "2026-08-03T13:00:00Z", "endAt": "2026-08-10T12:59:59.999Z",
        "game": { "id": "g1" },
        "self": { "isAccountConnected": true },
        "timeBasedDrops": []
    });
    assert!(parse_campaign(&v).is_err());
}

/// A null `allow.channels` means "any channel", not "no channels".
#[test]
fn a_null_channel_allow_list_means_any_channel() {
    let v = serde_json::json!({
        "id": "c1", "name": "X",
        "startAt": "2026-08-03T13:00:00Z", "endAt": "2026-08-10T12:59:59.999Z",
        "game": { "id": "g1", "name": "G", "slug": "g" },
        "self": { "isAccountConnected": true },
        "allow": { "channels": null },
        "timeBasedDrops": []
    });
    assert!(parse_campaign(&v).unwrap().allowed_channels.is_empty());
}

/// Real inventory data returns `preconditionDrops: null` rather than omitting it.
#[test]
fn null_precondition_drops_parse_as_no_preconditions() {
    let v = serde_json::json!({
        "id": "c1", "name": "X",
        "startAt": "2026-08-03T13:00:00Z", "endAt": "2026-08-10T12:59:59.999Z",
        "game": { "id": "g1", "name": "G", "slug": "g" },
        "self": { "isAccountConnected": true },
        "timeBasedDrops": [{
            "id": "d1", "name": "Drop 4", "requiredMinutesWatched": 240,
            "preconditionDrops": null, "benefitEdges": [],
            "self": { "currentMinutesWatched": 0, "isClaimed": false, "dropInstanceID": null }
        }]
    });
    assert!(parse_campaign(&v).unwrap().drops[0].precondition_ids.is_empty());
}

/// Twitch returns a campaign's tiers in whatever order it likes. A real
/// campaign came back as 240, 15, 30, 60, 120, 180 minutes — and since every
/// tier was also named "Madden Twitch Pack", the dashboard showed six
/// identical rewards at apparently random thresholds. Tier order is the only
/// thing that makes a tiered campaign readable.
#[test]
fn drops_are_put_into_tier_order() {
    let raw = serde_json::json!({
        "user": { "dropCampaign": {
            "id": "c1", "name": "Madden Live", "status": "ACTIVE",
            "startAt": "2026-08-01T00:00:00Z", "endAt": "2026-09-01T00:00:00Z",
            "game": { "id": "g1", "name": "Madden", "slug": "madden", "boxArtURL": null },
            "self": { "isAccountConnected": true },
            "timeBasedDrops": [
                { "id": "d240", "name": "Madden Twitch Pack", "requiredMinutesWatched": 240 },
                { "id": "d15",  "name": "Madden Twitch Pack", "requiredMinutesWatched": 15 },
                { "id": "d30",  "name": "Madden Twitch Pack", "requiredMinutesWatched": 30 },
                { "id": "d60",  "name": "Madden Twitch Pack", "requiredMinutesWatched": 60 },
            ],
        }}
    });
    let c = parse_campaign_details(&raw).unwrap().unwrap();
    let order: Vec<u32> = c.drops.iter().map(|d| d.required_minutes).collect();
    assert_eq!(order, vec![15, 30, 60, 240]);
}

/// A subscription reward arrives as a drop needing zero minutes and one sub.
/// Read without `requiredSubs` it is indistinguishable from a drop that is
/// already finished.
#[test]
fn a_subscription_reward_is_recognised_as_one() {
    let raw = serde_json::json!({
        "user": { "dropCampaign": {
            "id": "c1", "name": "LoL Classic Chat Badge", "status": "ACTIVE",
            "startAt": "2026-08-01T00:00:00Z", "endAt": "2026-09-01T00:00:00Z",
            "game": { "id": "g1", "name": "LoL", "slug": "lol", "boxArtURL": null },
            "self": { "isAccountConnected": true },
            "timeBasedDrops": [
                { "id": "d1", "name": "LoL Classic Chat Badge",
                  "requiredMinutesWatched": 0, "requiredSubs": 1 },
            ],
        }}
    });
    let c = parse_campaign_details(&raw).unwrap().unwrap();
    assert_eq!(c.drops[0].required_subs, 1);
    assert!(!c.drops[0].is_earnable_by_watching());
}

/// An ordinary drop must not be mistaken for one.
#[test]
fn an_ordinary_drop_needs_no_subscription() {
    let raw = serde_json::json!({
        "user": { "dropCampaign": {
            "id": "c1", "name": "Normal", "status": "ACTIVE",
            "startAt": "2026-08-01T00:00:00Z", "endAt": "2026-09-01T00:00:00Z",
            "game": { "id": "g1", "name": "G", "slug": "g", "boxArtURL": null },
            "self": { "isAccountConnected": true },
            "timeBasedDrops": [
                { "id": "d1", "name": "Tier 1", "requiredMinutesWatched": 60 },
            ],
        }}
    });
    let c = parse_campaign_details(&raw).unwrap().unwrap();
    assert_eq!(c.drops[0].required_subs, 0);
    assert!(c.drops[0].is_earnable_by_watching());
}

/// Sorting by minutes alone is wrong for the tier shape Twitch also uses: a
/// short finale gated behind two longer tiers. Minutes-ascending puts the
/// finale first, above the very tiers it depends on, labelled with a
/// requirement that ignores them.
#[test]
fn a_gated_tier_never_appears_before_what_gates_it() {
    let raw = serde_json::json!({
        "user": { "dropCampaign": {
            "id": "c1", "name": "Gated", "status": "ACTIVE",
            "startAt": "2026-08-01T00:00:00Z", "endAt": "2026-09-01T00:00:00Z",
            "game": { "id": "g1", "name": "G", "slug": "g", "boxArtURL": null },
            "self": { "isAccountConnected": true },
            "timeBasedDrops": [
                { "id": "final", "name": "Finale", "requiredMinutesWatched": 5,
                  "preconditionDrops": [{"id": "short"}, {"id": "long"}] },
                { "id": "long",  "name": "Long",  "requiredMinutesWatched": 100 },
                { "id": "short", "name": "Short", "requiredMinutesWatched": 10 },
            ],
        }}
    });
    let c = parse_campaign_details(&raw).unwrap().unwrap();
    let order: Vec<&str> = c.drops.iter().map(|d| d.id.0.as_str()).collect();
    assert_eq!(order, vec!["short", "long", "final"]);
}

/// A subscription reward needs zero minutes, so it would otherwise head every
/// mixed campaign — putting the one reward watching can never earn at the top
/// of the card.
#[test]
fn a_subscription_reward_does_not_head_the_list() {
    let raw = serde_json::json!({
        "user": { "dropCampaign": {
            "id": "c1", "name": "Mixed", "status": "ACTIVE",
            "startAt": "2026-08-01T00:00:00Z", "endAt": "2026-09-01T00:00:00Z",
            "game": { "id": "g1", "name": "G", "slug": "g", "boxArtURL": null },
            "self": { "isAccountConnected": true },
            "timeBasedDrops": [
                { "id": "sub", "name": "Sub badge",
                  "requiredMinutesWatched": 0, "requiredSubs": 1 },
                { "id": "t1", "name": "Tier 1", "requiredMinutesWatched": 60 },
            ],
        }}
    });
    let c = parse_campaign_details(&raw).unwrap().unwrap();
    assert_eq!(
        c.drops.iter().map(|d| d.id.0.as_str()).collect::<Vec<_>>(),
        vec!["t1", "sub"]
    );
}

/// Twitch data with a precondition cycle must not spin the ordering forever.
#[test]
fn a_precondition_cycle_terminates() {
    let raw = serde_json::json!({
        "user": { "dropCampaign": {
            "id": "c1", "name": "Cycle", "status": "ACTIVE",
            "startAt": "2026-08-01T00:00:00Z", "endAt": "2026-09-01T00:00:00Z",
            "game": { "id": "g1", "name": "G", "slug": "g", "boxArtURL": null },
            "self": { "isAccountConnected": true },
            "timeBasedDrops": [
                { "id": "a", "name": "A", "requiredMinutesWatched": 10,
                  "preconditionDrops": [{"id": "b"}] },
                { "id": "b", "name": "B", "requiredMinutesWatched": 20,
                  "preconditionDrops": [{"id": "a"}] },
            ],
        }}
    });
    let c = parse_campaign_details(&raw).unwrap().unwrap();
    assert_eq!(c.drops.len(), 2, "both tiers survive a cycle");
}
