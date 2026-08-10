//! The concrete [`TwitchApi`]. Everything here is transport; everything in the
//! engine above it is pure logic.

use crate::gql::GqlClient;
use crate::operations as ops;
use crate::parse;
use crate::watch::WatchClient;
use btdm_core::api::{ApiError, DropProgress, TwitchApi};
use btdm_core::model::*;
use serde_json::json;
use tokio::sync::Mutex;

pub struct TwitchClient {
    gql: GqlClient,
    watch: WatchClient,
    user_id: UserId,
    /// The account's own login, required as a variable by the campaign-details
    /// query even though the campaign is identified by ID.
    login: String,
    /// Discovered once, then reused; cheap to re-derive if it goes stale.
    spade_url: Mutex<Option<String>>,
}

impl TwitchClient {
    pub fn new(http: reqwest::Client, token: String, user_id: UserId, login: String) -> Self {
        Self {
            gql: GqlClient::new(http.clone(), Some(token)),
            watch: WatchClient::new(http),
            user_id,
            login,
            spade_url: Mutex::new(None),
        }
    }

    /// Test seam: point GQL at a mock server.
    pub fn with_endpoint(
        http: reqwest::Client,
        token: String,
        user_id: UserId,
        login: String,
        gql_endpoint: String,
    ) -> Self {
        Self {
            gql: GqlClient::with_endpoint(http.clone(), Some(token), gql_endpoint),
            watch: WatchClient::new(http),
            user_id,
            login,
            spade_url: Mutex::new(None),
        }
    }

    /// Forget the cached spade URL so the next heartbeat rediscovers it.
    pub async fn invalidate_spade_url(&self) {
        *self.spade_url.lock().await = None;
    }
}

impl TwitchApi for TwitchClient {
    async fn fetch_campaigns(&self) -> Result<Vec<DropCampaign>, ApiError> {
        let data = self.gql.run(&ops::CAMPAIGNS, json!({})).await?;
        Ok(parse::parse_campaigns(&data)?)
    }

    async fn fetch_inventory(&self) -> Result<Vec<DropCampaign>, ApiError> {
        let data = self.gql.run(&ops::INVENTORY, json!({})).await?;
        Ok(parse::parse_inventory(&data)?)
    }

    async fn campaign_details(&self, id: &CampaignId) -> Result<Option<DropCampaign>, ApiError> {
        let data = self
            .gql
            .run(
                &ops::CAMPAIGN_DETAILS,
                json!({ "dropID": id.0, "channelLogin": self.login }),
            )
            .await?;
        Ok(parse::parse_campaign_details(&data)?)
    }

    async fn available_drops(&self, channel: &ChannelId) -> Result<Vec<DropId>, ApiError> {
        let data = self
            .gql
            .run(&ops::AVAILABLE_DROPS, json!({ "channelID": channel.0 }))
            .await?;
        Ok(parse::parse_available_drops(&data))
    }

    async fn live_channels(
        &self,
        game: &Game,
        limit: u32,
    ) -> Result<Vec<(Channel, Stream)>, ApiError> {
        let data = self
            .gql
            .run(
                &ops::GAME_DIRECTORY,
                json!({
                    "slug": game.slug,
                    "limit": limit,
                    "options": {
                        "broadcasterLanguages": [],
                        "freeformTags": null,
                        "includeRestricted": ["SUB_ONLY_LIVE"],
                        "recommendationsContext": { "platform": "web" },
                        "sort": "RELEVANCE",
                        "systemFilters": [],
                        "tags": [],
                        "requestID": "JIRA-VXP-2397"
                    },
                    "sortTypeIsRecency": false,
                    "imageWidth": 50,
                    "includeCostreaming": false
                }),
            )
            .await?;
        Ok(parse::parse_directory(&data)?)
    }

    async fn stream_info(&self, login: &str) -> Result<Option<Stream>, ApiError> {
        let data = self
            .gql
            .run(&ops::STREAM_INFO, json!({ "channel": login }))
            .await?;
        Ok(parse::parse_stream_info(&data)?.map(|(_, stream)| stream))
    }

    async fn send_watch(&self, channel: &Channel, stream: &Stream) -> Result<(), ApiError> {
        let cached = self.spade_url.lock().await.clone();
        let url = match cached {
            Some(url) => url,
            None => {
                let url = self.watch.discover_spade_url(&channel.login).await?;
                *self.spade_url.lock().await = Some(url.clone());
                url
            }
        };

        match self.watch.send(&url, channel, stream, &self.user_id).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // A rotated deployment invalidates the cached URL. Drop it so
                // the next attempt rediscovers rather than failing forever.
                self.invalidate_spade_url().await;
                Err(e.into())
            }
        }
    }

    async fn current_progress(
        &self,
        _channel: &ChannelId,
    ) -> Result<Option<DropProgress>, ApiError> {
        let data = self.gql.run(&ops::CURRENT_DROP, json!({})).await?;
        Ok(parse::parse_current_progress(&data))
    }

    async fn claim_drop(&self, claim_id: &str) -> Result<(), ApiError> {
        self.gql
            .run(
                &ops::CLAIM_DROP,
                json!({ "input": { "dropInstanceID": claim_id } }),
            )
            .await?;
        Ok(())
    }
}
