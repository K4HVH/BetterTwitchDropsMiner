//! The GQL operations the miner uses.
//!
//! Each carries a persisted hash *and* raw query text. Hashes rot; the raw
//! query is what keeps the miner working the day one does.
//!
//! Every raw query must be valid standalone GraphQL. In particular, GraphQL's
//! "All Variables Used" validation rule rejects an operation that declares a
//! variable it never references — so the raw queries here declare only the
//! variables they actually use, even where the persisted query takes more.
//! Extra entries in the `variables` map are ignored by the server, so sending
//! the full set alongside a narrower raw query is safe.

use crate::gql::GqlOperation;

pub const INVENTORY: GqlOperation = GqlOperation {
    name: "Inventory",
    sha256: "8337eb8541b314040b0edde0c09c5c7a2783ba1960aa9edfbf3bac16d0fec404",
    raw_query: r#"query Inventory {
  currentUser {
    inventory {
      dropCampaignsInProgress {
        id name status startAt endAt accountLinkURL
        game { id name slug boxArtURL }
        self { isAccountConnected }
        allow { channels { id name displayName } }
        timeBasedDrops {
          id name requiredMinutesWatched startAt endAt
          benefitEdges { benefit { id name distributionType imageAssetURL } }
          self { currentMinutesWatched isClaimed dropInstanceID }
          preconditionDrops { id }
        }
      }
      gameEventDrops { id lastAwardedAt }
    }
  }
}"#,
};

pub const CAMPAIGNS: GqlOperation = GqlOperation {
    name: "ViewerDropsDashboard",
    sha256: "d9cae7761dafab85908c85e6683cb4201b449e66ac3bb5e894f15ff12aeafaa7",
    raw_query: r#"query ViewerDropsDashboard {
  currentUser {
    dropCampaigns {
      id name status startAt endAt accountLinkURL
      game { id name slug boxArtURL }
      self { isAccountConnected }
    }
  }
}"#,
};

pub const CAMPAIGN_DETAILS: GqlOperation = GqlOperation {
    name: "DropCampaignDetails",
    sha256: "039277bf98f3130929262cc7c6efd9c141ca3749cb6dca442fc8ead9a53f77c1",
    // `channelLogin` carries the user id (see `TwitchClient`), so the raw
    // query has to look the user up by id as well: `user(login:)` given an id
    // finds nobody.
    raw_query: r#"query DropCampaignDetails($dropID: ID!, $channelLogin: ID!) {
  user(id: $channelLogin) {
    dropCampaign(id: $dropID) {
      id name status startAt endAt accountLinkURL
      game { id name slug boxArtURL }
      self { isAccountConnected }
      allow { channels { id name displayName } }
      timeBasedDrops {
        id name requiredMinutesWatched startAt endAt
        benefitEdges { benefit { id name distributionType imageAssetURL } }
        self { currentMinutesWatched isClaimed dropInstanceID }
        preconditionDrops { id }
      }
    }
  }
}"#,
};

pub const AVAILABLE_DROPS: GqlOperation = GqlOperation {
    name: "DropsHighlightService_AvailableDrops",
    sha256: "782dad0f032942260171d2d80a654f88bdd0c5a9dddc392e9bc92218a0f42d20",
    raw_query: r#"query DropsHighlightService_AvailableDrops($channelID: ID!) {
  channel(id: $channelID) {
    viewerDropCampaigns {
      id
      timeBasedDrops { id requiredMinutesWatched self { currentMinutesWatched } }
    }
  }
}"#,
};

pub const CURRENT_DROP: GqlOperation = GqlOperation {
    name: "DropCurrentSessionContext",
    sha256: "4d06b702d25d652afb9ef835d2a550031f1cf762b193523a92166f40ea3d142b",
    // The persisted query takes channelID/channelLogin and scopes server-side;
    // the raw fallback cannot express that, so it returns whatever session the
    // account has open and the caller filters on the returned channel id.
    raw_query: r#"query DropCurrentSessionContext {
  currentUser {
    dropCurrentSession {
      channel { id }
      game { id name slug boxArtURL }
      currentMinutesWatched requiredMinutesWatched dropID
    }
  }
}"#,
};

pub const GAME_DIRECTORY: GqlOperation = GqlOperation {
    name: "DirectoryPage_Game",
    sha256: "86bcceb4e8b1a51256ff8eed8bd8aae4acacf80d737efe904f84f3aeadf8cafd",
    raw_query: r#"query DirectoryPage_Game($slug: String!, $limit: Int!, $options: GameStreamOptions) {
  game(slug: $slug) {
    id name slug
    streams(first: $limit, options: $options) {
      edges {
        node {
          id title viewersCount
          game { id name slug boxArtURL }
          broadcaster { id login displayName }
          freeformTags { name }
        }
      }
    }
  }
}"#,
};

pub const SLUG_REDIRECT: GqlOperation = GqlOperation {
    name: "DirectoryGameRedirect",
    sha256: "1f0300090caceec51f33c5e20647aceff9017f740f223c3c532ba6fa59f6b6cc",
    raw_query: r#"query DirectoryGameRedirect($name: String!) {
  game(name: $name) { id name slug }
}"#,
};

pub const STREAM_INFO: GqlOperation = GqlOperation {
    name: "VideoPlayerStreamInfoOverlayChannel",
    sha256: "198492e0857f6aedead9665c81c5a06d67b25b58034649687124083ff288597d",
    raw_query: r#"query VideoPlayerStreamInfoOverlayChannel($channel: String!) {
  user(login: $channel) {
    id login displayName
    stream { id viewersCount }
    broadcastSettings { title game { id name slug boxArtURL } }
  }
}"#,
};

/// Opens a stream for playback. Since 2026-10-08 this is the first step of
/// the only kind of watching Twitch credits; see [`crate::playback`].
pub const PLAYBACK_ACCESS_TOKEN: GqlOperation = GqlOperation {
    name: "PlaybackAccessToken",
    sha256: "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9",
    raw_query: r#"query PlaybackAccessToken($login: String!, $platform: String!, $playerType: String!) {
  streamPlaybackAccessToken(channelName: $login, params: {platform: $platform, playerBackend: "mediaplayer", playerType: $playerType}) {
    value signature
  }
}"#,
};

pub const CLAIM_DROP: GqlOperation = GqlOperation {
    name: "DropsPage_ClaimDropRewards",
    sha256: "a455deea71bdc9015b78eb49f4acfbce8baa7ccbedd28e549bb025bd0f751930",
    raw_query: r#"mutation DropsPage_ClaimDropRewards($input: ClaimDropRewardsInput!) {
  claimDropRewards(input: $input) { status }
}"#,
};

pub const CLAIM_POINTS: GqlOperation = GqlOperation {
    name: "ClaimCommunityPoints",
    sha256: "46aaeebe02c99afdf4fc97c7c0cba964124bf6b0af229395f1f6d1feed05b3d0",
    raw_query: r#"mutation ClaimCommunityPoints($input: ClaimCommunityPointsInput!) {
  claimCommunityPoints(input: $input) { currentPoints }
}"#,
};

pub const ALL: &[GqlOperation] = &[
    INVENTORY,
    CAMPAIGNS,
    CAMPAIGN_DETAILS,
    AVAILABLE_DROPS,
    CURRENT_DROP,
    GAME_DIRECTORY,
    SLUG_REDIRECT,
    STREAM_INFO,
    PLAYBACK_ACCESS_TOKEN,
    CLAIM_DROP,
    CLAIM_POINTS,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the variables an operation declares in its signature.
    fn declared_variables(raw: &str) -> Vec<String> {
        let Some(open) = raw.find('(') else {
            return Vec::new();
        };
        let Some(close) = raw[open..].find(')') else {
            return Vec::new();
        };
        raw[open + 1..open + close]
            .split(',')
            .filter_map(|part| {
                let t = part.trim();
                t.starts_with('$')
                    .then(|| t.split(':').next().unwrap_or(t).trim().to_owned())
            })
            .collect()
    }

    #[test]
    fn every_operation_is_well_formed() {
        for op in ALL {
            assert_eq!(op.sha256.len(), 64, "{}: hash must be 64 hex chars", op.name);
            assert!(
                op.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{}: hash must be hex",
                op.name
            );
            assert!(
                op.raw_query.contains(op.name),
                "{}: raw query must name the operation, or the fallback cannot work",
                op.name
            );
        }
    }

    /// GraphQL's "All Variables Used" rule rejects an operation declaring a
    /// variable it never references. A raw query that violates it would fail
    /// exactly when the persisted hash goes stale — i.e. when we need it most.
    #[test]
    fn every_declared_variable_is_referenced_in_the_body() {
        for op in ALL {
            let body_start = op.raw_query.find('{').expect("query has a body");
            let body = &op.raw_query[body_start..];
            for var in declared_variables(op.raw_query) {
                assert!(
                    body.contains(&var),
                    "{}: declares {var} but never uses it — Twitch will reject this fallback",
                    op.name
                );
            }
        }
    }

    #[test]
    fn braces_are_balanced() {
        for op in ALL {
            let opens = op.raw_query.matches('{').count();
            let closes = op.raw_query.matches('}').count();
            assert_eq!(opens, closes, "{}: unbalanced braces", op.name);
        }
    }

    #[test]
    fn operation_names_are_unique() {
        let mut names: Vec<_> = ALL.iter().map(|o| o.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate operation names");
    }

    #[test]
    fn the_variable_extractor_actually_works() {
        // Guards the test above from silently passing by finding nothing.
        assert_eq!(
            declared_variables("query X($a: ID!, $b: String!) { y }"),
            vec!["$a".to_string(), "$b".to_string()]
        );
        assert!(declared_variables("query X { y }").is_empty());
    }
}
