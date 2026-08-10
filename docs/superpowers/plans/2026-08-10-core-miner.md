# Core Miner Implementation Plan (Milestones 1–3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A headless, single-account Twitch drops miner that discovers campaigns, verifies eligibility against the server, watches a channel, tracks server-confirmed progress, and claims drops.

**Architecture:** A Cargo workspace of three crates. `btdm-core` holds the domain model and mining engine and is forbidden from making network calls — it talks to a `TwitchApi` trait. `btdm-twitch` implements that trait against Twitch's private GQL, spade and PubSub endpoints. `btdm` is the binary that wires them together. The network ban on `btdm-core` is what makes the failure paths testable offline.

**Tech Stack:** Rust (edition 2024), tokio, reqwest (rustls), tokio-tungstenite, rusqlite (bundled), serde, toml, tracing, thiserror. Tests use `wiremock` for HTTP and a hand-written `FakeTwitchApi` for engine scenarios.

**Scope:** Milestones 1–3 of the spec (`docs/superpowers/specs/2026-08-10-twitch-drops-miner-design.md`). The watchdog and escalation ladder (milestone 4), strategies (5), multi-account (6) and the web UI (7) are deliberately **out of scope** and get their own plans. This plan ends at "it mines".

## Global Constants

These are protocol facts. Copy them verbatim; getting one wrong causes silent failure.

| Name | Value |
|---|---|
| `CLIENT_ID` | `kd1unb4b3q4t58fwlpcbzcbnm76a8fp` (Twitch Android app — the device-code endpoint rejects the web client ID) |
| `USER_AGENT` | `Dalvik/2.1.0 (Linux; U; Android 16; SM-S911B Build/TP1A.220624.014) tv.twitch.android.app/25.3.0/2503006` |
| `GQL_URL` | `https://gql.twitch.tv/gql` |
| `DEVICE_URL` | `https://id.twitch.tv/oauth2/device` |
| `TOKEN_URL` | `https://id.twitch.tv/oauth2/token` |
| `VALIDATE_URL` | `https://id.twitch.tv/oauth2/validate` |
| `PUBSUB_URL` | `wss://pubsub-edge.twitch.tv/v1` |
| `DEVICE_CODE_GRANT` | `urn:ietf:params:oauth:grant-type:device_code` |
| Watch heartbeat | 60s |
| Progress poll backstop | 120s |

## Global Constraints

- Rust edition 2024, **MSRV 1.88** — the plan uses let-chains (`if let … && …`), which are not stable before that. Verify with `rustc --version` before starting.
- **`btdm-core` must not depend on `reqwest`, `tokio-tungstenite`, `rusqlite`, or any other I/O crate.** Task 1 adds a test that fails the build if it does. This is the plan's single most important invariant.
- TLS is `rustls` everywhere, never OpenSSL — the binary must stay statically linkable for aarch64.
- All timestamps are `chrono::DateTime<Utc>`. Never a naive local time.
- Every fallible public function returns `Result<T, E>` with a `thiserror` enum. No `unwrap()` or `expect()` outside tests and `main`.
- Files holding secrets (`config.toml`, `state.db`) are created mode `0600`.
- Run `cargo clippy --workspace --all-targets -- -D warnings` before every commit.

---

### Task 1: Workspace skeleton and the network-ban guard

Converts the existing single-package repo into a workspace and installs the invariant that keeps `btdm-core` testable.

**Files:**
- Modify: `Cargo.toml` (replace package with workspace)
- Delete: `src/main.rs`
- Create: `crates/btdm-core/Cargo.toml`, `crates/btdm-core/src/lib.rs`
- Create: `crates/btdm-twitch/Cargo.toml`, `crates/btdm-twitch/src/lib.rs`
- Create: `crates/btdm/Cargo.toml`, `crates/btdm/src/main.rs`
- Test: `crates/btdm-core/tests/no_network_deps.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: three crates named `btdm-core`, `btdm-twitch`, `btdm`. Every later task adds to these.

- [ ] **Step 1: Write the failing test**

`crates/btdm-core/tests/no_network_deps.rs` — this reads the lockfile and fails if a network crate ever enters `btdm-core`'s dependency tree.

```rust
use std::process::Command;

/// btdm-core must stay I/O-free so the engine can be tested without a network.
/// If this fails, someone added a networking dependency to the wrong crate.
#[test]
fn core_has_no_network_dependencies() {
    let output = Command::new("cargo")
        .args(["tree", "-p", "btdm-core", "--edges", "normal", "--prefix", "none"])
        .output()
        .expect("cargo tree should run");
    assert!(output.status.success(), "cargo tree failed");
    let tree = String::from_utf8_lossy(&output.stdout);

    for banned in ["reqwest", "hyper", "tokio-tungstenite", "rusqlite", "axum"] {
        assert!(
            !tree.lines().any(|l| l.trim_start().starts_with(banned)),
            "btdm-core must not depend on `{banned}` — it has to stay testable offline.\n{tree}"
        );
    }
}
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test -p btdm-core --test no_network_deps`
Expected: FAIL — the package `btdm-core` does not exist yet.

- [ ] **Step 3: Create the workspace**

Replace the root `Cargo.toml` entirely:

```toml
[workspace]
resolver = "3"
members = ["crates/btdm-core", "crates/btdm-twitch", "crates/btdm"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"

[workspace.dependencies]
chrono = { version = "0.4", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
```

Delete `src/main.rs` and remove the now-empty `src/` directory.

- [ ] **Step 4: Create the three crates**

`crates/btdm-core/Cargo.toml`:

```toml
[package]
name = "btdm-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
chrono.workspace = true
serde.workspace = true
thiserror.workspace = true
tracing.workspace = true
```

`crates/btdm-core/src/lib.rs`:

```rust
//! Domain model and mining engine. Contains no I/O by design.
```

`crates/btdm-twitch/Cargo.toml`:

```toml
[package]
name = "btdm-twitch"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
btdm-core = { path = "../btdm-core" }
chrono.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true
```

`crates/btdm-twitch/src/lib.rs`:

```rust
//! Twitch API client: auth, GQL, spade, PubSub.
```

`crates/btdm/Cargo.toml`:

```toml
[package]
name = "btdm"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
btdm-core = { path = "../btdm-core" }
btdm-twitch = { path = "../btdm-twitch" }
tokio.workspace = true
tracing.workspace = true
```

`crates/btdm/src/main.rs`:

```rust
fn main() {
    println!("btdm");
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p btdm-core --test no_network_deps`
Expected: PASS

Then `cargo build --workspace` — expected: succeeds.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: convert to workspace with core/twitch/bin crates

Adds a test that fails the build if btdm-core ever gains a network
dependency, which is what keeps the engine testable offline."
```

---

### Task 2: Domain model

The vocabulary every later task speaks. Pure data plus the arithmetic that operates on it.

**Files:**
- Create: `crates/btdm-core/src/model.rs`
- Modify: `crates/btdm-core/src/lib.rs`
- Test: inline `#[cfg(test)]` module in `model.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Game`, `Channel`, `Stream`, `Benefit`, `BenefitKind`, `TimedDrop`, `DropCampaign`, and the ID newtypes `ChannelId`, `GameId`, `CampaignId`, `DropId`, `UserId`. Methods used later: `TimedDrop::remaining_minutes() -> u32`, `TimedDrop::is_claimable() -> bool`, `DropCampaign::is_active(now) -> bool`, `DropCampaign::has_badge_or_emote() -> bool`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/btdm-core/src/model.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: i32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 8, 10, h as u32, 0, 0).unwrap()
    }

    fn drop_with(required: u32, current: u32) -> TimedDrop {
        TimedDrop {
            id: DropId("d1".into()),
            name: "Test Drop".into(),
            benefits: vec![],
            required_minutes: required,
            current_minutes: current,
            claim_id: None,
            is_claimed: false,
            precondition_ids: vec![],
        }
    }

    #[test]
    fn remaining_minutes_counts_down() {
        assert_eq!(drop_with(60, 23).remaining_minutes(), 37);
    }

    #[test]
    fn remaining_minutes_saturates_at_zero() {
        // Twitch occasionally reports more minutes than required.
        assert_eq!(drop_with(60, 75).remaining_minutes(), 0);
    }

    #[test]
    fn drop_is_claimable_only_with_a_claim_id_and_not_already_claimed() {
        let mut d = drop_with(60, 60);
        assert!(!d.is_claimable(), "no claim id yet");

        d.claim_id = Some("claim-1".into());
        assert!(d.is_claimable());

        d.is_claimed = true;
        assert!(!d.is_claimable(), "already claimed");
    }

    #[test]
    fn campaign_is_active_only_inside_its_window() {
        let c = DropCampaign {
            id: CampaignId("c1".into()),
            name: "Camp".into(),
            game: Game { id: GameId("g1".into()), name: "Game".into(), slug: "game".into() },
            starts_at: at(10),
            ends_at: at(20),
            drops: vec![],
            is_account_linked: true,
            allowed_channels: vec![],
        };
        assert!(!c.is_active(at(9)));
        assert!(c.is_active(at(15)));
        assert!(!c.is_active(at(21)));
    }

    #[test]
    fn campaign_detects_badge_and_emote_benefits() {
        let mut c = DropCampaign {
            id: CampaignId("c1".into()),
            name: "Camp".into(),
            game: Game { id: GameId("g1".into()), name: "Game".into(), slug: "game".into() },
            starts_at: at(10),
            ends_at: at(20),
            drops: vec![drop_with(60, 0)],
            is_account_linked: true,
            allowed_channels: vec![],
        };
        assert!(!c.has_badge_or_emote());

        c.drops[0].benefits.push(Benefit {
            id: "b1".into(),
            name: "Shiny Badge".into(),
            kind: BenefitKind::Badge,
        });
        assert!(c.has_badge_or_emote());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p btdm-core model`
Expected: FAIL — `model` module does not exist.

- [ ] **Step 3: Write the model**

Prepend to `crates/btdm-core/src/model.rs`:

```rust
//! The domain vocabulary. Pure data — no I/O, no Twitch JSON shapes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_owned()) }
        }
    )*};
}

id_type!(ChannelId, GameId, CampaignId, DropId, UserId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Game {
    pub id: GameId,
    pub name: String,
    /// URL slug, needed to query the game's channel directory.
    pub slug: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    /// Lowercase login name, used in URLs and GQL variables.
    pub login: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    /// Changes when a stream restarts. A stale value earns nothing while looking healthy.
    pub broadcast_id: String,
    pub game: Option<Game>,
    pub title: String,
    pub viewers: u32,
    /// Whether Twitch reports drops as enabled for this stream.
    pub drops_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BenefitKind {
    Badge,
    Emote,
    /// Anything else, including in-game items.
    Entitlement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Benefit {
    pub id: String,
    pub name: String,
    pub kind: BenefitKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedDrop {
    pub id: DropId,
    pub name: String,
    pub benefits: Vec<Benefit>,
    pub required_minutes: u32,
    /// Server-confirmed minutes. Never advanced by a local timer.
    pub current_minutes: u32,
    /// Present once the drop is earned and awaiting a claim.
    pub claim_id: Option<String>,
    pub is_claimed: bool,
    /// Drops that must be claimed before this tier unlocks.
    pub precondition_ids: Vec<DropId>,
}

impl TimedDrop {
    pub fn remaining_minutes(&self) -> u32 {
        self.required_minutes.saturating_sub(self.current_minutes)
    }

    pub fn is_claimable(&self) -> bool {
        !self.is_claimed && self.claim_id.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropCampaign {
    pub id: CampaignId,
    pub name: String,
    pub game: Game,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub drops: Vec<TimedDrop>,
    /// Whether Twitch reports this account as linked to the campaign.
    /// Never inferred from local settings — see Task 12.
    pub is_account_linked: bool,
    /// Channel allow-list. Empty means any channel streaming the game qualifies.
    pub allowed_channels: Vec<Channel>,
}

impl DropCampaign {
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        now >= self.starts_at && now < self.ends_at
    }

    pub fn has_badge_or_emote(&self) -> bool {
        self.drops.iter().flat_map(|d| &d.benefits).any(|b| {
            matches!(b.kind, BenefitKind::Badge | BenefitKind::Emote)
        })
    }
}
```

Add to `crates/btdm-core/src/lib.rs`:

```rust
pub mod model;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p btdm-core model`
Expected: PASS — 5 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm-core
git commit -m "feat(core): add domain model for campaigns, drops and channels"
```

---

### Task 3: The `TwitchApi` port and `MinerEvent`

The seam between the engine and the network. Everything the engine can ask Twitch to do lives here, so a fake can stand in for the real thing.

**Files:**
- Create: `crates/btdm-core/src/api.rs`, `crates/btdm-core/src/event.rs`
- Modify: `crates/btdm-core/src/lib.rs`
- Test: `crates/btdm-core/tests/fake_api.rs`

**Interfaces:**
- Consumes: `model::*` from Task 2.
- Produces: `trait TwitchApi` (async, `Send + Sync`) with methods `fetch_campaigns`, `fetch_inventory`, `available_drops`, `live_channels`, `stream_info`, `send_watch`, `current_progress`, `claim_drop`. Also `ApiError`, `MinerEvent`, `DropProgress`. Task 18's engine is generic over `TwitchApi`; Task 11–17 implement it in `btdm-twitch`.

- [ ] **Step 1: Write the failing test**

`crates/btdm-core/tests/fake_api.rs`:

```rust
use btdm_core::api::{ApiError, DropProgress, TwitchApi};
use btdm_core::model::*;

/// Proves the trait is object-safe-adjacent and implementable by a test double.
/// If this stops compiling, the engine can no longer be tested offline.
struct FakeApi;

impl TwitchApi for FakeApi {
    async fn fetch_campaigns(&self) -> Result<Vec<DropCampaign>, ApiError> {
        Ok(vec![])
    }
    async fn fetch_inventory(&self) -> Result<Vec<DropCampaign>, ApiError> {
        Ok(vec![])
    }
    async fn available_drops(&self, _channel: &ChannelId) -> Result<Vec<DropId>, ApiError> {
        Ok(vec![])
    }
    async fn live_channels(&self, _game: &Game, _limit: u32) -> Result<Vec<(Channel, Stream)>, ApiError> {
        Ok(vec![])
    }
    async fn stream_info(&self, _login: &str) -> Result<Option<Stream>, ApiError> {
        Ok(None)
    }
    async fn send_watch(&self, _channel: &Channel, _stream: &Stream) -> Result<(), ApiError> {
        Ok(())
    }
    async fn current_progress(&self, _channel: &ChannelId) -> Result<Option<DropProgress>, ApiError> {
        Ok(None)
    }
    async fn claim_drop(&self, _claim_id: &str) -> Result<(), ApiError> {
        Ok(())
    }
}

#[tokio::test]
async fn fake_api_satisfies_the_trait() {
    let api = FakeApi;
    assert!(api.fetch_campaigns().await.unwrap().is_empty());
    assert!(api.current_progress(&ChannelId::from("1")).await.unwrap().is_none());
}
```

Add dev-dependency: `cargo add --dev tokio --features macros,rt-multi-thread -p btdm-core`

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p btdm-core --test fake_api`
Expected: FAIL — `btdm_core::api` does not exist.

- [ ] **Step 3: Write the port and the event type**

`crates/btdm-core/src/api.rs`:

```rust
//! The seam between the mining engine and Twitch.
//!
//! The engine depends only on this trait, so tests can substitute a fake and
//! script failure scenarios that are impossible to trigger against the real API.

use crate::model::*;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("network failure: {0}")]
    Network(String),
    #[error("authentication is no longer valid")]
    Unauthorized,
    #[error("rate limited by Twitch")]
    RateLimited,
    #[error("unexpected response shape: {0}")]
    Malformed(String),
}

/// Server-confirmed progress for the drop currently being mined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropProgress {
    pub drop_id: DropId,
    pub current_minutes: u32,
    pub required_minutes: u32,
}

#[allow(async_fn_in_trait)]
pub trait TwitchApi: Send + Sync {
    /// All drop campaigns Twitch offers this account.
    async fn fetch_campaigns(&self) -> Result<Vec<DropCampaign>, ApiError>;

    /// Campaigns with in-progress or claimable drops for this account.
    async fn fetch_inventory(&self) -> Result<Vec<DropCampaign>, ApiError>;

    /// Which drops this channel can actually progress right now.
    /// This is the authoritative eligibility check.
    async fn available_drops(&self, channel: &ChannelId) -> Result<Vec<DropId>, ApiError>;

    async fn live_channels(&self, game: &Game, limit: u32) -> Result<Vec<(Channel, Stream)>, ApiError>;

    /// `None` when the channel is offline.
    async fn stream_info(&self, login: &str) -> Result<Option<Stream>, ApiError>;

    /// One watch heartbeat. The engine calls this on a fixed interval.
    async fn send_watch(&self, channel: &Channel, stream: &Stream) -> Result<(), ApiError>;

    /// Pull-based progress, the backstop for a live-but-silent websocket.
    async fn current_progress(&self, channel: &ChannelId) -> Result<Option<DropProgress>, ApiError>;

    async fn claim_drop(&self, claim_id: &str) -> Result<(), ApiError>;
}
```

`crates/btdm-core/src/event.rs`:

```rust
//! Everything the miner does is published as an event. The UI, the notifier
//! and the log are all consumers of this one stream.

use crate::model::*;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinerEvent {
    pub at: DateTime<Utc>,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// Login required; show this code to the user.
    AuthCodeIssued { user_code: String, verification_uri: String },
    AuthSucceeded { user_id: UserId, login: String },
    AuthExpired,
    CampaignsSynced { total: usize, eligible: usize },
    /// A campaign was rejected, with the reason, so the UI never has to guess.
    CampaignRejected { campaign: CampaignId, reason: RejectReason },
    NowWatching { channel: Channel, campaign: CampaignId },
    WatchSent { channel: ChannelId },
    WatchFailed { channel: ChannelId, error: String },
    /// Server-confirmed progress. The only source the UI is allowed to display.
    ProgressConfirmed { drop: DropId, current_minutes: u32, required_minutes: u32 },
    DropClaimed { drop: DropId, name: String },
    ChannelOffline { channel: ChannelId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// The account is not linked to this campaign — mining it can never progress.
    NotLinked,
    Expired,
    NotStarted,
    AllDropsClaimed,
    NoEligibleChannel,
}
```

Add to `crates/btdm-core/src/lib.rs`:

```rust
pub mod api;
pub mod event;
pub mod model;
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm-core --test fake_api`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/btdm-core
git commit -m "feat(core): add TwitchApi port and MinerEvent stream"
```

---

### Task 4: Configuration file

TOML is the source of truth for settings, rewritten atomically so a crash mid-write cannot corrupt it — the failure mode TDM papered over with "a more robust JSON save-load cycle".

**Files:**
- Create: `crates/btdm/src/config.rs`
- Modify: `crates/btdm/src/main.rs`
- Test: inline `#[cfg(test)]` in `config.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Config` with fields `bind: String`, `accounts: Vec<AccountConfig>`, `data_dir: Option<PathBuf>`; `AccountConfig { name: String, priority_games: Vec<String>, favourite_channels: Vec<String> }`. Functions `Config::load(&Path) -> Result<Config, ConfigError>` (returns defaults when absent) and `Config::save_atomic(&self, &Path) -> Result<(), ConfigError>`.

- [ ] **Step 1: Write the failing tests**

`crates/btdm/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:8080");
        assert!(cfg.accounts.is_empty());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config {
            bind: "0.0.0.0:9000".into(),
            data_dir: None,
            accounts: vec![AccountConfig {
                name: "main".into(),
                priority_games: vec!["Deadlock".into()],
                favourite_channels: vec!["somestreamer".into()],
            }],
        };
        cfg.save_atomic(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_atomic(&path).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn config_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_atomic(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "config may hold sensitive settings");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Add deps first:
```bash
cargo add toml serde --features serde/derive -p btdm
cargo add thiserror -p btdm
cargo add --dev tempfile -p btdm
```

Run: `cargo test -p btdm config`
Expected: FAIL — `Config` not found.

- [ ] **Step 3: Implement**

Prepend to `crates/btdm/src/config.rs`:

```rust
//! Settings live in TOML so Docker and systemd deployments stay declarative.
//! Runtime state lives in SQLite instead (see `storage`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config: {0}")]
    Io(#[from] std::io::Error),
    #[error("parsing config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("serialising config: {0}")]
    Serialise(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Loopback by default. Task 7 of the web-UI plan adds a token guard for
    /// any other bind address, because the state DB holds live OAuth tokens.
    pub bind: String,
    pub data_dir: Option<PathBuf>,
    pub accounts: Vec<AccountConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self { bind: "127.0.0.1:8080".into(), data_dir: None, accounts: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountConfig {
    pub name: String,
    pub priority_games: Vec<String>,
    pub favourite_channels: Vec<String>,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self { name: "default".into(), priority_games: Vec::new(), favourite_channels: Vec::new() }
    }
}

impl Config {
    /// A missing file is not an error — first run starts from defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write to a temp file, fsync, then rename. A crash mid-write leaves the
    /// previous config intact rather than a truncated one.
    pub fn save_atomic(&self, path: &Path) -> Result<(), ConfigError> {
        use std::io::Write;

        let text = toml::to_string_pretty(self)?;
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;

        let tmp = path.with_extension("toml.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}
```

Add to `crates/btdm/src/main.rs`:

```rust
mod config;

fn main() {
    println!("btdm");
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm config`
Expected: PASS — 4 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm
git commit -m "feat(btdm): add TOML config with atomic, 0600 writes"
```

---

### Task 5: SQLite state storage

Runtime state — tokens, caches, the event log — in a store that cannot half-write.

**Files:**
- Create: `crates/btdm/src/storage.rs`
- Modify: `crates/btdm/src/main.rs`
- Test: inline `#[cfg(test)]` in `storage.rs`

**Interfaces:**
- Consumes: `btdm_core::event::MinerEvent`.
- Produces: `Storage::open(&Path) -> Result<Storage, StorageError>`, `Storage::open_in_memory()`, `Storage::put_tokens(account, &Tokens)`, `Storage::get_tokens(account) -> Option<Tokens>`, `Storage::append_event(account, &MinerEvent)`, `Storage::recent_events(account, limit) -> Vec<(DateTime<Utc>, String)>`. `Tokens { access_token: String, refresh_token: Option<String>, user_id: String, login: String }`.

- [ ] **Step 1: Write the failing tests**

`crates/btdm/src/storage.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> Tokens {
        Tokens {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            user_id: "12345".into(),
            login: "someone".into(),
        }
    }

    #[test]
    fn tokens_round_trip() {
        let s = Storage::open_in_memory().unwrap();
        assert!(s.get_tokens("main").unwrap().is_none());
        s.put_tokens("main", &tokens()).unwrap();
        assert_eq!(s.get_tokens("main").unwrap().unwrap(), tokens());
    }

    #[test]
    fn tokens_are_replaced_not_duplicated() {
        let s = Storage::open_in_memory().unwrap();
        s.put_tokens("main", &tokens()).unwrap();
        let mut updated = tokens();
        updated.access_token = "new".into();
        s.put_tokens("main", &updated).unwrap();
        assert_eq!(s.get_tokens("main").unwrap().unwrap().access_token, "new");
    }

    #[test]
    fn accounts_do_not_share_tokens() {
        let s = Storage::open_in_memory().unwrap();
        s.put_tokens("main", &tokens()).unwrap();
        assert!(s.get_tokens("alt").unwrap().is_none());
    }

    #[test]
    fn events_come_back_newest_first_and_respect_the_limit() {
        use btdm_core::event::{EventKind, MinerEvent};
        let s = Storage::open_in_memory().unwrap();
        for i in 0..5 {
            s.append_event("main", &MinerEvent {
                at: chrono::Utc::now() + chrono::Duration::seconds(i),
                kind: EventKind::WatchSent { channel: btdm_core::model::ChannelId::from("c1") },
            }).unwrap();
        }
        let got = s.recent_events("main", 3).unwrap();
        assert_eq!(got.len(), 3);
        assert!(got[0].0 >= got[1].0, "expected newest first");
    }

    #[cfg(unix)]
    #[test]
    fn database_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let _s = Storage::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the DB holds live OAuth tokens");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo add rusqlite --features bundled -p btdm
cargo add chrono --features serde -p btdm
cargo add btdm-core --path crates/btdm-core -p btdm
```

Run: `cargo test -p btdm storage`
Expected: FAIL — `Storage` not found.

- [ ] **Step 3: Implement**

Prepend to `crates/btdm/src/storage.rs`:

```rust
//! Runtime state. SQLite rather than JSON because a half-written JSON file is
//! a corrupted config, whereas a half-written transaction is simply rolled back.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub user_id: String,
    pub login: String,
}

pub struct Storage {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tokens (
    account       TEXT PRIMARY KEY,
    access_token  TEXT NOT NULL,
    refresh_token TEXT,
    user_id       TEXT NOT NULL,
    login         TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    account TEXT NOT NULL,
    at      TEXT NOT NULL,
    kind    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_by_account ON events(account, id DESC);
";

impl Storage {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StorageError> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn put_tokens(&self, account: &str, t: &Tokens) -> Result<(), StorageError> {
        self.lock().execute(
            "INSERT INTO tokens (account, access_token, refresh_token, user_id, login)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account) DO UPDATE SET
                access_token=excluded.access_token,
                refresh_token=excluded.refresh_token,
                user_id=excluded.user_id,
                login=excluded.login",
            params![account, t.access_token, t.refresh_token, t.user_id, t.login],
        )?;
        Ok(())
    }

    pub fn get_tokens(&self, account: &str) -> Result<Option<Tokens>, StorageError> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT access_token, refresh_token, user_id, login FROM tokens WHERE account = ?1",
                params![account],
                |r| {
                    Ok(Tokens {
                        access_token: r.get(0)?,
                        refresh_token: r.get(1)?,
                        user_id: r.get(2)?,
                        login: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn append_event(
        &self,
        account: &str,
        event: &btdm_core::event::MinerEvent,
    ) -> Result<(), StorageError> {
        self.lock().execute(
            "INSERT INTO events (account, at, kind) VALUES (?1, ?2, ?3)",
            params![account, event.at.to_rfc3339(), format!("{:?}", event.kind)],
        )?;
        Ok(())
    }

    pub fn recent_events(
        &self,
        account: &str,
        limit: u32,
    ) -> Result<Vec<(DateTime<Utc>, String)>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT at, kind FROM events WHERE account = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![account, limit], |r| {
                let at: String = r.get(0)?;
                let kind: String = r.get(1)?;
                Ok((at, kind))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(at, kind)| {
                DateTime::parse_from_rfc3339(&at)
                    .ok()
                    .map(|t| (t.with_timezone(&Utc), kind))
            })
            .collect())
    }
}
```

Add `mod storage;` to `crates/btdm/src/main.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm storage`
Expected: PASS — 5 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm
git commit -m "feat(btdm): add SQLite storage for tokens and the event log"
```

---

### Task 6: GQL transport with raw-query fallback

The differentiator against TDM: a stale persisted-query hash becomes a logged warning instead of a silent breakage.

**Files:**
- Create: `crates/btdm-twitch/src/gql.rs`, `crates/btdm-twitch/src/client.rs`
- Modify: `crates/btdm-twitch/src/lib.rs`
- Test: `crates/btdm-twitch/tests/gql_fallback.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `GqlOperation { name: &'static str, sha256: &'static str, raw_query: &'static str }`; `GqlClient::new(http: reqwest::Client, token: Option<String>)`; `GqlClient::run(&self, op: &GqlOperation, variables: serde_json::Value) -> Result<serde_json::Value, TwitchError>` returning the `data` object. `TwitchError` enum. `build_http_client() -> reqwest::Client` applying `CLIENT_ID` and `USER_AGENT`.

- [ ] **Step 1: Write the failing tests**

`crates/btdm-twitch/tests/gql_fallback.rs`:

```rust
use btdm_twitch::gql::{GqlClient, GqlOperation};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const OP: GqlOperation = GqlOperation {
    name: "TestOp",
    sha256: "deadbeef",
    raw_query: "query TestOp { viewer { id } }",
};

#[tokio::test]
async fn uses_the_persisted_hash_when_twitch_accepts_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "viewer": { "id": "1" } }
        })))
        .mount(&server)
        .await;

    let client = GqlClient::with_endpoint(
        reqwest::Client::new(),
        None,
        format!("{}/gql", server.uri()),
    );
    let data = client.run(&OP, json!({})).await.unwrap();
    assert_eq!(data["viewer"]["id"], "1");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "no fallback should have been needed");
    let body: serde_json::Value = reqs[0].body_json().unwrap();
    assert_eq!(body["extensions"]["persistedQuery"]["sha256Hash"], "deadbeef");
    assert!(body.get("query").is_none(), "must not send raw query first");
}

#[tokio::test]
async fn falls_back_to_the_raw_query_when_the_hash_is_stale() {
    let server = MockServer::start().await;

    // Twitch reports a stale hash as a GraphQL error, not an HTTP error.
    let responder = |req: &Request| {
        let body: serde_json::Value = req.body_json().unwrap();
        if body.get("query").is_some() {
            ResponseTemplate::new(200).set_body_json(json!({
                "data": { "viewer": { "id": "1" } }
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{ "message": "PersistedQueryNotFound" }]
            }))
        }
    };
    Mock::given(method("POST")).and(path("/gql")).respond_with(responder).mount(&server).await;

    let client = GqlClient::with_endpoint(
        reqwest::Client::new(),
        None,
        format!("{}/gql", server.uri()),
    );
    let data = client.run(&OP, json!({})).await.unwrap();
    assert_eq!(data["viewer"]["id"], "1", "fallback should have succeeded");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2, "expected a hash attempt then a raw retry");
    let second: serde_json::Value = reqs[1].body_json().unwrap();
    assert_eq!(second["query"], OP.raw_query);
}

#[tokio::test]
async fn a_real_graphql_error_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{ "message": "service error" }]
        })))
        .mount(&server)
        .await;

    let client = GqlClient::with_endpoint(
        reqwest::Client::new(),
        None,
        format!("{}/gql", server.uri()),
    );
    assert!(client.run(&OP, json!({})).await.is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 1, "must not retry");
}

#[tokio::test]
async fn http_401_maps_to_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client = GqlClient::with_endpoint(
        reqwest::Client::new(),
        None,
        format!("{}/gql", server.uri()),
    );
    assert!(matches!(
        client.run(&OP, json!({})).await,
        Err(btdm_twitch::TwitchError::Unauthorized)
    ));
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo add reqwest --no-default-features --features json,rustls-tls,gzip -p btdm-twitch
cargo add --dev wiremock tokio --features tokio/macros,tokio/rt-multi-thread -p btdm-twitch
```

Run: `cargo test -p btdm-twitch --test gql_fallback`
Expected: FAIL — `btdm_twitch::gql` does not exist.

- [ ] **Step 3: Implement the client and transport**

`crates/btdm-twitch/src/client.rs`:

```rust
//! Shared HTTP setup. Every request presents as the Twitch Android app,
//! because that is the client the device-code auth endpoint accepts.

pub const CLIENT_ID: &str = "kd1unb4b3q4t58fwlpcbzcbnm76a8fp";
pub const USER_AGENT: &str = "Dalvik/2.1.0 (Linux; U; Android 16; SM-S911B Build/TP1A.220624.014) tv.twitch.android.app/25.3.0/2503006";
pub const GQL_URL: &str = "https://gql.twitch.tv/gql";

/// A random 32-char lowercase hex device ID, stable for the process lifetime.
pub fn generate_device_id() -> String {
    use std::hash::{BuildHasher, RandomState};
    // Two 64-bit values give 128 bits => 32 hex chars.
    let a = RandomState::new().hash_one("btdm-device-a");
    let b = RandomState::new().hash_one("btdm-device-b");
    format!("{a:016x}{b:016x}")
}

pub fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .gzip(true)
        .build()
}
```

`crates/btdm-twitch/src/gql.rs`:

```rust
//! GQL transport.
//!
//! Twitch's client uses persisted queries identified by a SHA-256 hash. Those
//! hashes change without notice; TDM ships hardcoded ones and breaks quietly
//! when they rot. Here every operation also carries its raw query text, so a
//! `PersistedQueryNotFound` triggers an immediate retry instead of a failure.

use crate::TwitchError;
use crate::client::{CLIENT_ID, GQL_URL};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy)]
pub struct GqlOperation {
    pub name: &'static str,
    pub sha256: &'static str,
    pub raw_query: &'static str,
}

pub struct GqlClient {
    http: reqwest::Client,
    token: Option<String>,
    endpoint: String,
}

impl GqlClient {
    pub fn new(http: reqwest::Client, token: Option<String>) -> Self {
        Self::with_endpoint(http, token, GQL_URL.to_owned())
    }

    /// Test seam: point the client at a mock server.
    pub fn with_endpoint(http: reqwest::Client, token: Option<String>, endpoint: String) -> Self {
        Self { http, token, endpoint }
    }

    /// Runs `op`, returning its `data` object.
    pub async fn run(&self, op: &GqlOperation, variables: Value) -> Result<Value, TwitchError> {
        let persisted = json!({
            "operationName": op.name,
            "variables": variables,
            "extensions": { "persistedQuery": { "version": 1, "sha256Hash": op.sha256 } },
        });

        match self.post(&persisted).await {
            Err(TwitchError::StaleQueryHash) => {
                tracing::warn!(
                    operation = op.name,
                    "persisted query hash is stale; falling back to the raw query"
                );
                let raw = json!({
                    "operationName": op.name,
                    "variables": variables,
                    "query": op.raw_query,
                });
                self.post(&raw).await
            }
            other => other,
        }
    }

    async fn post(&self, body: &Value) -> Result<Value, TwitchError> {
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("Client-Id", CLIENT_ID)
            .json(body);
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("OAuth {token}"));
        }

        let resp = req.send().await.map_err(|e| TwitchError::Network(e.to_string()))?;
        match resp.status().as_u16() {
            200 => {}
            401 | 403 => return Err(TwitchError::Unauthorized),
            429 => return Err(TwitchError::RateLimited),
            s => return Err(TwitchError::Network(format!("HTTP {s}"))),
        }

        let json: Value = resp
            .json()
            .await
            .map_err(|e| TwitchError::Malformed(e.to_string()))?;

        if let Some(errors) = json.get("errors").and_then(Value::as_array)
            && !errors.is_empty()
        {
            let stale = errors.iter().any(|e| {
                e.get("message").and_then(Value::as_str) == Some("PersistedQueryNotFound")
            });
            return Err(if stale {
                TwitchError::StaleQueryHash
            } else {
                TwitchError::Gql(errors.to_vec())
            });
        }

        json.get("data")
            .cloned()
            .ok_or_else(|| TwitchError::Malformed("response had no `data`".into()))
    }
}
```

`crates/btdm-twitch/src/lib.rs`:

```rust
//! Twitch API client: auth, GQL, spade, PubSub.

pub mod client;
pub mod gql;

#[derive(Debug, thiserror::Error)]
pub enum TwitchError {
    #[error("network failure: {0}")]
    Network(String),
    #[error("authentication is no longer valid")]
    Unauthorized,
    #[error("rate limited by Twitch")]
    RateLimited,
    #[error("unexpected response shape: {0}")]
    Malformed(String),
    #[error("persisted query hash is stale")]
    StaleQueryHash,
    #[error("graphql errors: {0:?}")]
    Gql(Vec<serde_json::Value>),
}

impl From<TwitchError> for btdm_core::api::ApiError {
    fn from(e: TwitchError) -> Self {
        use btdm_core::api::ApiError;
        match e {
            TwitchError::Unauthorized => ApiError::Unauthorized,
            TwitchError::RateLimited => ApiError::RateLimited,
            TwitchError::Network(m) => ApiError::Network(m),
            other => ApiError::Malformed(other.to_string()),
        }
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm-twitch --test gql_fallback`
Expected: PASS — 4 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm-twitch
git commit -m "feat(twitch): GQL transport with raw-query fallback for stale hashes

A stale persisted-query hash is the failure mode that breaks TDM silently.
Here it degrades to a logged warning and an automatic retry."
```

---

### Task 7: Operation catalogue

Every GQL operation the miner needs, each with both a hash and raw query text.

**Files:**
- Create: `crates/btdm-twitch/src/operations.rs`
- Modify: `crates/btdm-twitch/src/lib.rs`
- Test: inline `#[cfg(test)]` in `operations.rs`

**Interfaces:**
- Consumes: `GqlOperation` from Task 6.
- Produces: consts `INVENTORY`, `CAMPAIGNS`, `CAMPAIGN_DETAILS`, `AVAILABLE_DROPS`, `CURRENT_DROP`, `GAME_DIRECTORY`, `SLUG_REDIRECT`, `STREAM_INFO`, `CLAIM_DROP`, `CLAIM_POINTS`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_is_well_formed() {
        let ops = [
            INVENTORY, CAMPAIGNS, CAMPAIGN_DETAILS, AVAILABLE_DROPS, CURRENT_DROP,
            GAME_DIRECTORY, SLUG_REDIRECT, STREAM_INFO, CLAIM_DROP, CLAIM_POINTS,
        ];
        for op in ops {
            assert_eq!(op.sha256.len(), 64, "{}: hash must be 64 hex chars", op.name);
            assert!(
                op.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{}: hash must be hex", op.name
            );
            assert!(
                op.raw_query.contains(op.name),
                "{}: raw query must name the operation, or the fallback cannot work",
                op.name
            );
        }
    }

    #[test]
    fn operation_names_are_unique() {
        let names = [
            INVENTORY.name, CAMPAIGNS.name, CAMPAIGN_DETAILS.name, AVAILABLE_DROPS.name,
            CURRENT_DROP.name, GAME_DIRECTORY.name, SLUG_REDIRECT.name, STREAM_INFO.name,
            CLAIM_DROP.name, CLAIM_POINTS.name,
        ];
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate operation names");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p btdm-twitch operations`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

Hashes are taken from the reference implementation at `/home/user/Documents/miner/TwitchDropsMiner/constants.py`, which is the current known-good set. Raw queries request only the fields Task 11 parses.

```rust
//! The GQL operations the miner uses.
//!
//! Each carries a persisted hash *and* raw query text. Hashes rot; the raw
//! query is what keeps the miner working the day one does.

use crate::gql::GqlOperation;

pub const INVENTORY: GqlOperation = GqlOperation {
    name: "Inventory",
    sha256: "8337eb8541b314040b0edde0c09c5c7a2783ba1960aa9edfbf3bac16d0fec404",
    raw_query: r#"query Inventory($fetchRewardCampaigns: Boolean = false) {
  currentUser {
    inventory {
      dropCampaignsInProgress {
        id name status startAt endAt
        game { id name slug }
        self { isAccountConnected }
        allow { channels { id name displayName } }
        timeBasedDrops {
          id name requiredMinutesWatched
          benefitEdges { benefit { id name distributionType } }
          self { currentMinutesWatched isClaimed dropInstanceID }
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
    raw_query: r#"query ViewerDropsDashboard($fetchRewardCampaigns: Boolean = false) {
  currentUser {
    dropCampaigns {
      id name status startAt endAt
      game { id name slug }
      self { isAccountConnected }
    }
  }
}"#,
};

pub const CAMPAIGN_DETAILS: GqlOperation = GqlOperation {
    name: "DropCampaignDetails",
    sha256: "039277bf98f3130929262cc7c6efd9c141ca3749cb6dca442fc8ead9a53f77c1",
    raw_query: r#"query DropCampaignDetails($dropID: ID!, $channelLogin: String!) {
  user(login: $channelLogin) {
    dropCampaign(id: $dropID) {
      id name status startAt endAt
      game { id name slug }
      self { isAccountConnected }
      allow { channels { id name displayName } }
      timeBasedDrops {
        id name requiredMinutesWatched
        benefitEdges { benefit { id name distributionType } }
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
    raw_query: r#"query DropCurrentSessionContext($channelID: ID!, $channelLogin: String!) {
  currentUser {
    dropCurrentSession {
      channel { id }
      game { id name slug }
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
          game { id name slug }
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
    broadcastSettings { title game { id name slug } }
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
```

Add `pub mod operations;` to `crates/btdm-twitch/src/lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm-twitch operations`
Expected: PASS — 2 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm-twitch
git commit -m "feat(twitch): add GQL operation catalogue with raw-query fallbacks"
```

---

### Task 8: Device-code authentication

**Files:**
- Create: `crates/btdm-twitch/src/auth.rs`
- Modify: `crates/btdm-twitch/src/lib.rs`
- Test: `crates/btdm-twitch/tests/auth_flow.rs`

**Interfaces:**
- Consumes: `client::{CLIENT_ID, generate_device_id}`, `TwitchError`.
- Produces: `DeviceCode { device_code, user_code, verification_uri, interval_secs, expires_in_secs }`; `AuthClient::with_endpoints(http, device_url, token_url, validate_url)`; `AuthClient::start_device_flow() -> Result<DeviceCode, TwitchError>`; `AuthClient::poll_once(&DeviceCode) -> Result<PollOutcome, TwitchError>` where `PollOutcome` is `Pending | Granted { access_token, refresh_token }`; `AuthClient::validate(token) -> Result<ValidatedToken { user_id, login }, TwitchError>`.

- [ ] **Step 1: Write the failing tests**

`crates/btdm-twitch/tests/auth_flow.rs`:

```rust
use btdm_twitch::auth::{AuthClient, PollOutcome};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> AuthClient {
    AuthClient::with_endpoints(
        reqwest::Client::new(),
        format!("{}/device", server.uri()),
        format!("{}/token", server.uri()),
        format!("{}/validate", server.uri()),
    )
}

#[tokio::test]
async fn start_device_flow_returns_the_user_code() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc-40-chars",
            "user_code": "ABCDEFGH",
            "verification_uri": "https://www.twitch.tv/activate?device-code=ABCDEFGH",
            "interval": 5,
            "expires_in": 1800
        })))
        .mount(&server)
        .await;

    let dc = client(&server).start_device_flow().await.unwrap();
    assert_eq!(dc.user_code, "ABCDEFGH");
    assert_eq!(dc.interval_secs, 5);
    assert!(dc.verification_uri.contains("activate"));
}

#[tokio::test]
async fn polling_before_the_user_authorises_is_pending_not_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": "authorization_pending"
        })))
        .mount(&server)
        .await;

    let dc = btdm_twitch::auth::DeviceCode {
        device_code: "dc".into(),
        user_code: "ABCDEFGH".into(),
        verification_uri: "https://example.invalid".into(),
        interval_secs: 5,
        expires_in_secs: 1800,
    };
    assert!(matches!(
        client(&server).poll_once(&dc).await.unwrap(),
        PollOutcome::Pending
    ));
}

#[tokio::test]
async fn polling_after_authorisation_yields_a_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-40-chars",
            "refresh_token": "rt-40-chars",
            "token_type": "bearer"
        })))
        .mount(&server)
        .await;

    let dc = btdm_twitch::auth::DeviceCode {
        device_code: "dc".into(),
        user_code: "ABCDEFGH".into(),
        verification_uri: "https://example.invalid".into(),
        interval_secs: 5,
        expires_in_secs: 1800,
    };
    match client(&server).poll_once(&dc).await.unwrap() {
        PollOutcome::Granted { access_token, refresh_token } => {
            assert_eq!(access_token, "at-40-chars");
            assert_eq!(refresh_token.as_deref(), Some("rt-40-chars"));
        }
        PollOutcome::Pending => panic!("expected a granted token"),
    }
}

#[tokio::test]
async fn validate_extracts_the_user_identity() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "client_id": "kd1unb4b3q4t58fwlpcbzcbnm76a8fp",
            "login": "someone",
            "user_id": "12345",
            "expires_in": 5000000
        })))
        .mount(&server)
        .await;

    let v = client(&server).validate("at").await.unwrap();
    assert_eq!(v.user_id, "12345");
    assert_eq!(v.login, "someone");
}

#[tokio::test]
async fn validate_reports_a_revoked_token_as_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/validate"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    assert!(matches!(
        client(&server).validate("at").await,
        Err(btdm_twitch::TwitchError::Unauthorized)
    ));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p btdm-twitch --test auth_flow`
Expected: FAIL — `btdm_twitch::auth` does not exist.

- [ ] **Step 3: Implement**

`crates/btdm-twitch/src/auth.rs`:

```rust
//! OAuth 2.0 device-code flow.
//!
//! The user is shown an eight-character code to enter at twitch.tv/activate.
//! No browser automation and no cookie jar — which is also what makes this
//! workable on a headless machine.

use crate::TwitchError;
use crate::client::CLIENT_ID;
use serde::Deserialize;

pub const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
pub const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
pub const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
pub const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval_secs: u64,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// The user has not entered the code yet. Wait `interval_secs` and retry.
    Pending,
    Granted { access_token: String, refresh_token: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedToken {
    pub user_id: String,
    pub login: String,
}

pub struct AuthClient {
    http: reqwest::Client,
    device_url: String,
    token_url: String,
    validate_url: String,
}

impl AuthClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self::with_endpoints(
            http,
            DEVICE_URL.to_owned(),
            TOKEN_URL.to_owned(),
            VALIDATE_URL.to_owned(),
        )
    }

    /// Test seam: point the flow at a mock server.
    pub fn with_endpoints(
        http: reqwest::Client,
        device_url: String,
        token_url: String,
        validate_url: String,
    ) -> Self {
        Self { http, device_url, token_url, validate_url }
    }

    pub async fn start_device_flow(&self) -> Result<DeviceCode, TwitchError> {
        #[derive(Deserialize)]
        struct Resp {
            device_code: String,
            user_code: String,
            verification_uri: String,
            interval: u64,
            expires_in: u64,
        }

        let resp = self
            .http
            .post(&self.device_url)
            .header("Client-Id", CLIENT_ID)
            .form(&[("client_id", CLIENT_ID), ("scopes", "")])
            .send()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(TwitchError::Network(format!("HTTP {}", resp.status())));
        }
        let r: Resp = resp.json().await.map_err(|e| TwitchError::Malformed(e.to_string()))?;
        Ok(DeviceCode {
            device_code: r.device_code,
            user_code: r.user_code,
            verification_uri: r.verification_uri,
            interval_secs: r.interval,
            expires_in_secs: r.expires_in,
        })
    }

    /// One poll. Twitch answers 400 until the user authorises, which is normal.
    pub async fn poll_once(&self, dc: &DeviceCode) -> Result<PollOutcome, TwitchError> {
        #[derive(Deserialize)]
        struct Resp {
            access_token: String,
            refresh_token: Option<String>,
        }

        let resp = self
            .http
            .post(&self.token_url)
            .header("Client-Id", CLIENT_ID)
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", &dc.device_code),
                ("grant_type", DEVICE_CODE_GRANT),
            ])
            .send()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        if resp.status().as_u16() == 200 {
            let r: Resp = resp.json().await.map_err(|e| TwitchError::Malformed(e.to_string()))?;
            return Ok(PollOutcome::Granted {
                access_token: r.access_token,
                refresh_token: r.refresh_token,
            });
        }
        Ok(PollOutcome::Pending)
    }

    pub async fn validate(&self, token: &str) -> Result<ValidatedToken, TwitchError> {
        #[derive(Deserialize)]
        struct Resp {
            user_id: String,
            login: String,
        }

        let resp = self
            .http
            .get(&self.validate_url)
            .header("Authorization", format!("OAuth {token}"))
            .send()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        match resp.status().as_u16() {
            200 => {
                let r: Resp = resp.json().await.map_err(|e| TwitchError::Malformed(e.to_string()))?;
                Ok(ValidatedToken { user_id: r.user_id, login: r.login })
            }
            401 | 403 => Err(TwitchError::Unauthorized),
            s => Err(TwitchError::Network(format!("HTTP {s}"))),
        }
    }
}
```

Add `pub mod auth;` to `crates/btdm-twitch/src/lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm-twitch --test auth_flow`
Expected: PASS — 5 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm-twitch
git commit -m "feat(twitch): add OAuth device-code authentication"
```

---

### Task 9: Campaign and inventory parsing

Turns Twitch's JSON into the domain model. Fixture-driven so parsing regressions surface offline.

**Files:**
- Create: `crates/btdm-twitch/src/parse.rs`
- Create: `crates/btdm-twitch/tests/fixtures/inventory.json`, `crates/btdm-twitch/tests/fixtures/campaigns.json`
- Test: `crates/btdm-twitch/tests/parse_fixtures.rs`

**Interfaces:**
- Consumes: `btdm_core::model::*`.
- Produces: `parse_inventory(&Value) -> Result<Vec<DropCampaign>, TwitchError>`, `parse_campaigns(&Value) -> Result<Vec<DropCampaign>, TwitchError>`, `parse_directory(&Value) -> Result<Vec<(Channel, Stream)>, TwitchError>`, `parse_stream_info(&Value) -> Result<Option<(Channel, Stream)>, TwitchError>`, `parse_benefit_kind(&str) -> BenefitKind`.

> **Note for the implementer:** these fixtures are reconstructed from the reference implementation's parsing code (`/home/user/Documents/miner/TwitchDropsMiner/inventory.py`), which is the best available description of the response shape. Once Task 15 runs against a live account, capture real responses and replace these fixtures. That replacement is a task in the milestone-4 plan, not this one.

- [ ] **Step 1: Write the fixture**

`crates/btdm-twitch/tests/fixtures/inventory.json`:

```json
{
  "currentUser": {
    "inventory": {
      "dropCampaignsInProgress": [
        {
          "id": "camp-1",
          "name": "Deadlock Launch Drops",
          "status": "ACTIVE",
          "startAt": "2026-08-01T00:00:00Z",
          "endAt": "2026-09-01T00:00:00Z",
          "game": { "id": "g-1", "name": "Deadlock", "slug": "deadlock" },
          "self": { "isAccountConnected": true },
          "allow": { "channels": [{ "id": "ch-1", "name": "streamer1", "displayName": "Streamer1" }] },
          "timeBasedDrops": [
            {
              "id": "drop-1",
              "name": "One Hour Reward",
              "requiredMinutesWatched": 60,
              "benefitEdges": [
                { "benefit": { "id": "b-1", "name": "Skin", "distributionType": "DIRECT_ENTITLEMENT" } }
              ],
              "self": { "currentMinutesWatched": 23, "isClaimed": false, "dropInstanceID": null }
            },
            {
              "id": "drop-2",
              "name": "Three Hour Reward",
              "requiredMinutesWatched": 180,
              "benefitEdges": [
                { "benefit": { "id": "b-2", "name": "Emote", "distributionType": "EMOTE" } }
              ],
              "self": { "currentMinutesWatched": 180, "isClaimed": false, "dropInstanceID": "inst-2" }
            }
          ]
        },
        {
          "id": "camp-2",
          "name": "Unlinked Badge Campaign",
          "status": "ACTIVE",
          "startAt": "2026-08-01T00:00:00Z",
          "endAt": "2026-09-01T00:00:00Z",
          "game": { "id": "g-2", "name": "Other Game", "slug": "other-game" },
          "self": { "isAccountConnected": false },
          "allow": { "channels": [] },
          "timeBasedDrops": [
            {
              "id": "drop-3",
              "name": "Badge",
              "requiredMinutesWatched": 120,
              "benefitEdges": [
                { "benefit": { "id": "b-3", "name": "Shiny Badge", "distributionType": "BADGE" } }
              ],
              "self": { "currentMinutesWatched": 0, "isClaimed": false, "dropInstanceID": null }
            }
          ]
        }
      ]
    }
  }
}
```

- [ ] **Step 2: Write the failing tests**

`crates/btdm-twitch/tests/parse_fixtures.rs`:

```rust
use btdm_core::model::*;
use btdm_twitch::parse::parse_inventory;

fn inventory() -> Vec<DropCampaign> {
    let raw = include_str!("fixtures/inventory.json");
    let value: serde_json::Value = serde_json::from_str(raw).unwrap();
    parse_inventory(&value).unwrap()
}

#[test]
fn parses_every_campaign() {
    assert_eq!(inventory().len(), 2);
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
fn account_link_state_is_taken_from_the_server_never_assumed() {
    let inv = inventory();
    assert!(inv[0].is_account_linked);
    assert!(!inv[1].is_account_linked, "server says unlinked");
}

#[test]
fn allowed_channels_are_parsed() {
    let inv = inventory();
    assert_eq!(inv[0].allowed_channels.len(), 1);
    assert_eq!(inv[0].allowed_channels[0].login, "streamer1");
    assert!(inv[1].allowed_channels.is_empty(), "empty means any channel");
}

#[test]
fn malformed_input_errors_rather_than_panicking() {
    let value = serde_json::json!({ "currentUser": null });
    assert!(parse_inventory(&value).is_err());
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p btdm-twitch --test parse_fixtures`
Expected: FAIL — `btdm_twitch::parse` does not exist.

- [ ] **Step 4: Implement**

`crates/btdm-twitch/src/parse.rs`:

```rust
//! Twitch JSON to domain model.
//!
//! Kept separate from transport so fixtures can exercise it without a network,
//! and so a schema change shows up as a failing parse test rather than a
//! mysterious runtime behaviour.

use crate::TwitchError;
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

fn parse_game(v: &Value) -> Result<Game, TwitchError> {
    Ok(Game {
        id: GameId(str_at(v, "id")?.to_owned()),
        name: str_at(v, "name")?.to_owned(),
        // Twitch omits `slug` in some shapes; derive it rather than failing.
        slug: v
            .get("slug")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| str_at(v, "name").unwrap_or_default().to_lowercase().replace(' ', "-")),
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
    let selfv = v.get("self");
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
                            b.get("distributionType").and_then(Value::as_str).unwrap_or(""),
                        ),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(TimedDrop {
        id: DropId(str_at(v, "id")?.to_owned()),
        name: v.get("name").and_then(Value::as_str).unwrap_or("").to_owned(),
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

fn parse_campaign(v: &Value) -> Result<DropCampaign, TwitchError> {
    let game = parse_game(v.get("game").ok_or_else(|| malformed("campaign has no game"))?)?;
    let drops = v
        .get("timeBasedDrops")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_drop).collect::<Result<Vec<_>, _>>())
        .transpose()?
        .unwrap_or_default();
    let allowed_channels = v
        .get("allow")
        .and_then(|a| a.get("channels"))
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_channel).collect::<Result<Vec<_>, _>>())
        .transpose()?
        .unwrap_or_default();

    Ok(DropCampaign {
        id: CampaignId(str_at(v, "id")?.to_owned()),
        name: v.get("name").and_then(Value::as_str).unwrap_or("").to_owned(),
        game,
        starts_at: time_at(v, "startAt")?,
        ends_at: time_at(v, "endAt")?,
        drops,
        // Absent `self` means Twitch did not confirm a link, so treat it as unlinked.
        is_account_linked: v
            .get("self")
            .and_then(|s| s.get("isAccountConnected"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        allowed_channels,
    })
}

fn campaign_list(data: &Value, path: &[&str]) -> Result<Vec<DropCampaign>, TwitchError> {
    let mut node = data;
    for key in path {
        node = node
            .get(key)
            .filter(|v| !v.is_null())
            .ok_or_else(|| malformed(&format!("missing `{key}`")))?;
    }
    node.as_array()
        .ok_or_else(|| malformed("expected an array of campaigns"))?
        .iter()
        .map(parse_campaign)
        .collect()
}

pub fn parse_inventory(data: &Value) -> Result<Vec<DropCampaign>, TwitchError> {
    campaign_list(data, &["currentUser", "inventory", "dropCampaignsInProgress"])
}

pub fn parse_campaigns(data: &Value) -> Result<Vec<DropCampaign>, TwitchError> {
    campaign_list(data, &["currentUser", "dropCampaigns"])
}

pub fn parse_directory(data: &Value) -> Result<Vec<(Channel, Stream)>, TwitchError> {
    let edges = data
        .get("game")
        .filter(|v| !v.is_null())
        .and_then(|g| g.get("streams"))
        .and_then(|s| s.get("edges"))
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("directory has no stream edges"))?;

    let mut out = Vec::new();
    for edge in edges {
        let Some(node) = edge.get("node") else { continue };
        let Some(bc) = node.get("broadcaster") else { continue };
        let Ok(channel) = parse_channel(bc) else { continue };
        let game = node.get("game").and_then(|g| parse_game(g).ok());
        let drops_enabled = node
            .get("freeformTags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter().any(|t| {
                    t.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.eq_ignore_ascii_case("DropsEnabled"))
                })
            })
            .unwrap_or(false);

        out.push((
            channel,
            Stream {
                broadcast_id: node.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
                game,
                title: node.get("title").and_then(Value::as_str).unwrap_or("").to_owned(),
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
    let settings = user.get("broadcastSettings");

    Ok(Some((
        channel,
        Stream {
            broadcast_id: stream.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
            game: settings.and_then(|s| s.get("game")).and_then(|g| parse_game(g).ok()),
            title: settings
                .and_then(|s| s.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            viewers: stream.get("viewersCount").and_then(Value::as_u64).unwrap_or(0) as u32,
            // Stream info does not carry tags; Task 12 verifies eligibility properly.
            drops_enabled: true,
        },
    )))
}
```

Add `pub mod parse;` to `crates/btdm-twitch/src/lib.rs`.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p btdm-twitch --test parse_fixtures`
Expected: PASS — 7 tests.

- [ ] **Step 6: Commit**

```bash
git add crates/btdm-twitch
git commit -m "feat(twitch): parse campaigns, inventory and directories from GQL"
```

---

### Task 10: Eligibility rules

The regression test for TDM's badges/emotes bug lives here. This is the task that makes the miner honest.

**Files:**
- Create: `crates/btdm-core/src/eligibility.rs`
- Modify: `crates/btdm-core/src/lib.rs`
- Test: inline `#[cfg(test)]` in `eligibility.rs`

**Interfaces:**
- Consumes: `model::*`, `event::RejectReason`.
- Produces: `assess(&DropCampaign, now: DateTime<Utc>) -> Result<(), RejectReason>` and `mineable_drops(&DropCampaign) -> Vec<&TimedDrop>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, h, 0, 0).unwrap()
    }

    fn campaign(linked: bool, benefit: BenefitKind, claimed: bool) -> DropCampaign {
        DropCampaign {
            id: CampaignId::from("c1"),
            name: "Camp".into(),
            game: Game { id: GameId::from("g1"), name: "Game".into(), slug: "game".into() },
            starts_at: at(10),
            ends_at: at(20),
            is_account_linked: linked,
            allowed_channels: vec![],
            drops: vec![TimedDrop {
                id: DropId::from("d1"),
                name: "Drop".into(),
                benefits: vec![Benefit { id: "b1".into(), name: "Thing".into(), kind: benefit }],
                required_minutes: 60,
                current_minutes: 0,
                claim_id: None,
                is_claimed: claimed,
                precondition_ids: vec![],
            }],
        }
    }

    #[test]
    fn a_linked_active_campaign_is_eligible() {
        assert_eq!(assess(&campaign(true, BenefitKind::Entitlement, false), at(15)), Ok(()));
    }

    /// This is the TDM bug, pinned. In TDM, enabling the badges/emotes option
    /// made badge campaigns eligible *without* checking the account link, so the
    /// miner ground away at campaigns that could never progress.
    #[test]
    fn a_badge_campaign_is_still_rejected_when_the_account_is_not_linked() {
        assert_eq!(
            assess(&campaign(false, BenefitKind::Badge, false), at(15)),
            Err(RejectReason::NotLinked)
        );
        assert_eq!(
            assess(&campaign(false, BenefitKind::Emote, false), at(15)),
            Err(RejectReason::NotLinked)
        );
    }

    #[test]
    fn a_linked_badge_campaign_is_eligible() {
        assert_eq!(assess(&campaign(true, BenefitKind::Badge, false), at(15)), Ok(()));
    }

    #[test]
    fn campaigns_outside_their_window_are_rejected_with_the_reason() {
        assert_eq!(assess(&campaign(true, BenefitKind::Entitlement, false), at(9)), Err(RejectReason::NotStarted));
        assert_eq!(assess(&campaign(true, BenefitKind::Entitlement, false), at(21)), Err(RejectReason::Expired));
    }

    #[test]
    fn a_fully_claimed_campaign_is_rejected() {
        assert_eq!(
            assess(&campaign(true, BenefitKind::Entitlement, true), at(15)),
            Err(RejectReason::AllDropsClaimed)
        );
    }

    #[test]
    fn mineable_drops_skips_claimed_and_blocked_tiers() {
        let mut c = campaign(true, BenefitKind::Entitlement, false);
        c.drops.push(TimedDrop {
            id: DropId::from("d2"),
            name: "Tier 2".into(),
            benefits: vec![],
            required_minutes: 180,
            current_minutes: 0,
            claim_id: None,
            is_claimed: false,
            // Blocked until d1 is claimed.
            precondition_ids: vec![DropId::from("d1")],
        });
        let ids: Vec<_> = mineable_drops(&c).iter().map(|d| d.id.clone()).collect();
        assert_eq!(ids, vec![DropId::from("d1")], "tier 2 is gated behind tier 1");

        c.drops[0].is_claimed = true;
        let ids: Vec<_> = mineable_drops(&c).iter().map(|d| d.id.clone()).collect();
        assert_eq!(ids, vec![DropId::from("d2")], "tier 2 unlocks once tier 1 is claimed");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p btdm-core eligibility`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

```rust
//! Whether a campaign can actually be mined.
//!
//! Every rule here is derived from server-reported facts. Nothing is inferred
//! from local settings — that inference is exactly what made the reference
//! implementation mine campaigns it could never progress.

use crate::event::RejectReason;
use crate::model::*;
use chrono::{DateTime, Utc};

pub fn assess(campaign: &DropCampaign, now: DateTime<Utc>) -> Result<(), RejectReason> {
    if now < campaign.starts_at {
        return Err(RejectReason::NotStarted);
    }
    if now >= campaign.ends_at {
        return Err(RejectReason::Expired);
    }
    // The server is the only authority on whether this account is linked.
    // There is deliberately no settings flag that can override this.
    if !campaign.is_account_linked {
        return Err(RejectReason::NotLinked);
    }
    if mineable_drops(campaign).is_empty() {
        return Err(RejectReason::AllDropsClaimed);
    }
    Ok(())
}

/// Drops that can be progressed right now: unclaimed, and with every
/// precondition tier already claimed.
pub fn mineable_drops(campaign: &DropCampaign) -> Vec<&TimedDrop> {
    let claimed: std::collections::HashSet<&DropId> = campaign
        .drops
        .iter()
        .filter(|d| d.is_claimed)
        .map(|d| &d.id)
        .collect();

    campaign
        .drops
        .iter()
        .filter(|d| !d.is_claimed)
        .filter(|d| d.precondition_ids.iter().all(|p| claimed.contains(p)))
        .collect()
}
```

Add `pub mod eligibility;` to `crates/btdm-core/src/lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm-core eligibility`
Expected: PASS — 6 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm-core
git commit -m "feat(core): server-verified eligibility rules

Pins the TDM defect: a badge campaign the account is not linked to is
rejected regardless of any local setting."
```

---

### Task 11: Spade URL discovery and the watch heartbeat

**Files:**
- Create: `crates/btdm-twitch/src/watch.rs`
- Modify: `crates/btdm-twitch/src/lib.rs`
- Test: `crates/btdm-twitch/tests/watch.rs`

**Interfaces:**
- Consumes: `TwitchError`, `btdm_core::model::{Channel, Stream, UserId}`.
- Produces: `extract_spade_url(html: &str) -> Option<String>`, `extract_settings_url(html: &str) -> Option<String>`, `build_watch_payload(channel, stream, user_id, now) -> String` (base64 of the minute-watched JSON), `WatchClient::send(&self, spade_url, channel, stream, user_id) -> Result<(), TwitchError>`.

- [ ] **Step 1: Write the failing tests**

`crates/btdm-twitch/tests/watch.rs`:

```rust
use btdm_core::model::*;
use btdm_twitch::watch::{build_watch_payload, extract_settings_url, extract_spade_url};

#[test]
fn finds_a_spade_url_embedded_directly_in_the_page() {
    // The mobile page exposes it inline, which saves a second request.
    let html = r#"<script>var x = {"spade_url":"https://video-edge-1.pdx.abs.hls.ttvnw.net/v1/event"};</script>"#;
    assert_eq!(
        extract_spade_url(html).as_deref(),
        Some("https://video-edge-1.pdx.abs.hls.ttvnw.net/v1/event")
    );
}

#[test]
fn tolerates_the_spaced_and_underscore_free_spellings() {
    assert!(extract_spade_url(r#"{"spadeUrl": "https://a.example/v1/e"}"#).is_some());
    assert!(extract_spade_url(r#"{"spade_url" : "https://a.example/v1/e"}"#).is_some());
}

#[test]
fn finds_the_settings_js_url_when_the_page_lacks_the_spade_url() {
    let html = r#"<script src="https://static.twitchcdn.net/config/settings.0123456789abcdef0123456789abcdef.js"></script>"#;
    assert!(extract_spade_url(html).is_none());
    assert_eq!(
        extract_settings_url(html).as_deref(),
        Some("https://static.twitchcdn.net/config/settings.0123456789abcdef0123456789abcdef.js")
    );
}

#[test]
fn watch_payload_carries_the_fields_twitch_counts_on() {
    let channel = Channel {
        id: ChannelId::from("12345"),
        login: "streamer1".into(),
        display_name: "Streamer1".into(),
    };
    let stream = Stream {
        broadcast_id: "bc-99".into(),
        game: Some(Game { id: GameId::from("g1"), name: "Deadlock".into(), slug: "deadlock".into() }),
        title: "t".into(),
        viewers: 10,
        drops_enabled: true,
    };
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-10T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let encoded = build_watch_payload(&channel, &stream, &UserId::from("999"), now);
    let decoded = String::from_utf8(
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &encoded).unwrap(),
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&decoded).unwrap();

    assert_eq!(parsed[0]["event"], "minute-watched");
    let p = &parsed[0]["properties"];
    assert_eq!(p["channel_id"], "12345");
    assert_eq!(p["channel"], "streamer1");
    assert_eq!(p["broadcast_id"], "bc-99", "a stale broadcast id earns nothing");
    assert_eq!(p["game"], "Deadlock");
    assert_eq!(p["game_id"], "g1");
    assert_eq!(p["user_id"], "999");
    assert_eq!(p["live"], true);
    assert_eq!(p["logged_in"], true);
    assert_eq!(p["minutes_logged"], 1);
}

#[test]
fn watch_payload_handles_a_stream_with_no_game() {
    let channel = Channel {
        id: ChannelId::from("1"),
        login: "s".into(),
        display_name: "S".into(),
    };
    let stream = Stream {
        broadcast_id: "b".into(),
        game: None,
        title: "t".into(),
        viewers: 0,
        drops_enabled: false,
    };
    let now = chrono::Utc::now();
    let encoded = build_watch_payload(&channel, &stream, &UserId::from("9"), now);
    let decoded = String::from_utf8(
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &encoded).unwrap(),
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&decoded).unwrap();
    assert_eq!(parsed[0]["properties"]["game"], "");
    assert_eq!(parsed[0]["properties"]["game_id"], "");
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo add base64 regex -p btdm-twitch
cargo add --dev base64 serde_json -p btdm-twitch
```

Run: `cargo test -p btdm-twitch --test watch`
Expected: FAIL — `btdm_twitch::watch` does not exist.

- [ ] **Step 3: Implement**

`crates/btdm-twitch/src/watch.rs`:

```rust
//! The watch heartbeat.
//!
//! Twitch counts drop time from "minute-watched" analytics events posted to a
//! per-deployment "spade" endpoint. The endpoint URL is not in any API — it is
//! embedded in the site's JavaScript config and has to be scraped. The mobile
//! page exposes it inline, so we fetch that first and skip a hop.

use crate::TwitchError;
use base64::Engine;
use btdm_core::model::{Channel, Stream, UserId};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::sync::LazyLock;

pub const MOBILE_CHANNEL_URL: &str = "https://m.twitch.tv";

static SPADE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#""spade_?[Uu]rl"\s*:\s*"(https://[.\w\-/]+)""#).expect("valid regex")
});

static SETTINGS_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"src="(https://[\w.\-]+/config/settings\.[0-9a-f]{32}\.js)""#)
        .expect("valid regex")
});

pub fn extract_spade_url(html: &str) -> Option<String> {
    SPADE_RE.captures(html).map(|c| c[1].to_owned())
}

pub fn extract_settings_url(html: &str) -> Option<String> {
    SETTINGS_RE.captures(html).map(|c| c[1].to_owned())
}

/// The base64-encoded analytics payload Twitch expects in the `data` form field.
pub fn build_watch_payload(
    channel: &Channel,
    stream: &Stream,
    user_id: &UserId,
    now: DateTime<Utc>,
) -> String {
    let (game_name, game_id) = match &stream.game {
        Some(g) => (g.name.clone(), g.id.0.clone()),
        None => (String::new(), String::new()),
    };

    let payload = json!([{
        "event": "minute-watched",
        "properties": {
            "broadcast_id": stream.broadcast_id,
            "channel_id": channel.id.0,
            "channel": channel.login,
            "client_time": now.timestamp() as f64,
            "game": game_name,
            "game_id": game_id,
            "hidden": false,
            "is_live": true,
            "live": true,
            "logged_in": true,
            "minutes_logged": 1,
            "muted": false,
            "user_id": user_id.0,
        }
    }]);

    base64::engine::general_purpose::STANDARD.encode(payload.to_string())
}

pub struct WatchClient {
    http: reqwest::Client,
}

impl WatchClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    /// Walks the chain page -> (settings JS) -> spade URL.
    pub async fn discover_spade_url(&self, channel_login: &str) -> Result<String, TwitchError> {
        let page_url = format!("{MOBILE_CHANNEL_URL}/{channel_login}");
        let html = self
            .http
            .get(&page_url)
            .send()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?
            .text()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        if let Some(url) = extract_spade_url(&html) {
            return Ok(url);
        }

        let settings_url = extract_settings_url(&html)
            .ok_or_else(|| TwitchError::Malformed("no spade or settings URL in page".into()))?;
        let js = self
            .http
            .get(&settings_url)
            .send()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?
            .text()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        extract_spade_url(&js)
            .ok_or_else(|| TwitchError::Malformed("no spade URL in settings JS".into()))
    }

    /// One heartbeat. Twitch answers 204 on success.
    pub async fn send(
        &self,
        spade_url: &str,
        channel: &Channel,
        stream: &Stream,
        user_id: &UserId,
    ) -> Result<(), TwitchError> {
        let data = build_watch_payload(channel, stream, user_id, Utc::now());
        let resp = self
            .http
            .post(spade_url)
            .form(&[("data", data)])
            .send()
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        match resp.status().as_u16() {
            200 | 204 => Ok(()),
            401 | 403 => Err(TwitchError::Unauthorized),
            429 => Err(TwitchError::RateLimited),
            s => Err(TwitchError::Network(format!("spade returned HTTP {s}"))),
        }
    }
}
```

Add `pub mod watch;` to `crates/btdm-twitch/src/lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm-twitch --test watch`
Expected: PASS — 5 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm-twitch
git commit -m "feat(twitch): spade URL discovery and minute-watched heartbeat"
```

---

### Task 12: Confirmed-progress tracker

The honesty layer. Merges pushed and polled progress and records when each drop was last confirmed, so a stall is measurable rather than invisible.

**Files:**
- Create: `crates/btdm-core/src/progress.rs`
- Modify: `crates/btdm-core/src/lib.rs`
- Test: inline `#[cfg(test)]` in `progress.rs`

**Interfaces:**
- Consumes: `model::DropId`, `api::DropProgress`.
- Produces: `ProgressTracker::new()`, `record(&mut self, DropProgress, at: DateTime<Utc>) -> bool` (true when it advanced), `confirmed(&self, &DropId) -> Option<&ConfirmedProgress>`, `since_last_confirmation(&self, now) -> Option<Duration>`. `ConfirmedProgress { current_minutes, required_minutes, at }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, 12, m, 0).unwrap()
    }

    fn progress(current: u32) -> DropProgress {
        DropProgress {
            drop_id: DropId::from("d1"),
            current_minutes: current,
            required_minutes: 60,
        }
    }

    #[test]
    fn the_first_report_counts_as_advancing() {
        let mut t = ProgressTracker::new();
        assert!(t.record(progress(5), at(0)));
        assert_eq!(t.confirmed(&DropId::from("d1")).unwrap().current_minutes, 5);
    }

    #[test]
    fn a_higher_reading_advances() {
        let mut t = ProgressTracker::new();
        t.record(progress(5), at(0));
        assert!(t.record(progress(6), at(1)));
    }

    #[test]
    fn a_repeated_reading_does_not_advance() {
        let mut t = ProgressTracker::new();
        t.record(progress(5), at(0));
        assert!(!t.record(progress(5), at(1)), "same minute count is not progress");
    }

    /// Twitch sometimes reports progress for the wrong drop, or replays an old
    /// value. Going backwards must never be mistaken for progress, and must not
    /// corrupt the confirmed figure.
    #[test]
    fn a_lower_reading_is_ignored() {
        let mut t = ProgressTracker::new();
        t.record(progress(10), at(0));
        assert!(!t.record(progress(4), at(1)));
        assert_eq!(t.confirmed(&DropId::from("d1")).unwrap().current_minutes, 10);
    }

    #[test]
    fn tracks_time_since_the_last_real_advance() {
        let mut t = ProgressTracker::new();
        assert_eq!(t.since_last_confirmation(at(5)), None, "nothing confirmed yet");

        t.record(progress(5), at(0));
        assert_eq!(t.since_last_confirmation(at(3)), Some(Duration::minutes(3)));

        // A non-advancing report must not reset the clock — that is precisely
        // how a stall stays invisible.
        t.record(progress(5), at(3));
        assert_eq!(t.since_last_confirmation(at(6)), Some(Duration::minutes(6)));

        t.record(progress(6), at(7));
        assert_eq!(t.since_last_confirmation(at(8)), Some(Duration::minutes(1)));
    }

    #[test]
    fn drops_are_tracked_independently() {
        let mut t = ProgressTracker::new();
        t.record(progress(5), at(0));
        t.record(
            DropProgress { drop_id: DropId::from("d2"), current_minutes: 1, required_minutes: 30 },
            at(1),
        );
        assert_eq!(t.confirmed(&DropId::from("d1")).unwrap().current_minutes, 5);
        assert_eq!(t.confirmed(&DropId::from("d2")).unwrap().current_minutes, 1);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p btdm-core progress`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

```rust
//! Server-confirmed progress.
//!
//! The reference implementation shows a locally-ticking countdown that advances
//! whether or not Twitch is counting anything, which is why its users cannot
//! tell mining from not-mining. Nothing here advances without a server report,
//! and `since_last_confirmation` makes a stall a measurable quantity.

use crate::api::DropProgress;
use crate::model::DropId;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedProgress {
    pub current_minutes: u32,
    pub required_minutes: u32,
    /// When this figure last actually increased.
    pub at: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct ProgressTracker {
    drops: HashMap<DropId, ConfirmedProgress>,
    last_advance: Option<DateTime<Utc>>,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` only when this report advanced a drop. Re-reports and
    /// backwards readings return `false` and leave the confirmed figure alone.
    pub fn record(&mut self, p: DropProgress, at: DateTime<Utc>) -> bool {
        let advanced = match self.drops.get(&p.drop_id) {
            Some(existing) => p.current_minutes > existing.current_minutes,
            None => true,
        };

        if advanced {
            self.drops.insert(
                p.drop_id,
                ConfirmedProgress {
                    current_minutes: p.current_minutes,
                    required_minutes: p.required_minutes,
                    at,
                },
            );
            self.last_advance = Some(at);
        }
        advanced
    }

    pub fn confirmed(&self, drop: &DropId) -> Option<&ConfirmedProgress> {
        self.drops.get(drop)
    }

    /// How long since anything actually advanced. `None` before the first
    /// confirmation. This is what the watchdog in milestone 4 will threshold on.
    pub fn since_last_confirmation(&self, now: DateTime<Utc>) -> Option<Duration> {
        self.last_advance.map(|t| now - t)
    }

    /// Forget everything — used when switching channels, since progress on the
    /// old channel says nothing about the new one.
    pub fn reset(&mut self) {
        self.drops.clear();
        self.last_advance = None;
    }
}
```

Add `pub mod progress;` to `crates/btdm-core/src/lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm-core progress`
Expected: PASS — 6 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm-core
git commit -m "feat(core): server-confirmed progress tracker

Only a genuine increase counts as progress, so time-since-confirmation
turns a silent stall into a number."
```

---

### Task 13: PubSub websocket client

**Files:**
- Create: `crates/btdm-twitch/src/pubsub.rs`
- Modify: `crates/btdm-twitch/src/lib.rs`
- Test: `crates/btdm-twitch/tests/pubsub.rs`

**Interfaces:**
- Consumes: `TwitchError`, `btdm_core::api::DropProgress`.
- Produces: `Topic::{UserDrops, StreamState}` with `fn as_str(&self, id: &str) -> String`; `PubSubMessage::{DropProgress(DropProgress), DropClaimable{drop_id, claim_id}, StreamUp{channel_id}, StreamDown{channel_id}, PointsClaimable{channel_id, claim_id}}`; `parse_pubsub_frame(&str) -> Option<PubSubMessage>`; `PubSubClient::connect(url, token, topics) -> Result<Receiver<PubSubMessage>, TwitchError>`.

- [ ] **Step 1: Write the failing tests**

`crates/btdm-twitch/tests/pubsub.rs`:

```rust
use btdm_twitch::pubsub::{PubSubMessage, Topic, parse_pubsub_frame};

#[test]
fn topics_render_with_their_target_id() {
    assert_eq!(Topic::UserDrops.as_str("999"), "user-drop-events.999");
    assert_eq!(Topic::StreamState.as_str("12345"), "video-playback-by-id.12345");
    assert_eq!(Topic::UserPoints.as_str("999"), "community-points-user-v1.999");
}

#[test]
fn parses_a_drop_progress_message() {
    // PubSub nests a JSON string inside a JSON envelope.
    let inner = serde_json::json!({
        "type": "drop-progress",
        "data": { "drop_id": "d1", "current_progress_min": 23, "required_progress_min": 60 }
    })
    .to_string();
    let frame = serde_json::json!({
        "type": "MESSAGE",
        "data": { "topic": "user-drop-events.999", "message": inner }
    })
    .to_string();

    match parse_pubsub_frame(&frame) {
        Some(PubSubMessage::DropProgress(p)) => {
            assert_eq!(p.drop_id.0, "d1");
            assert_eq!(p.current_minutes, 23);
            assert_eq!(p.required_minutes, 60);
        }
        other => panic!("expected drop progress, got {other:?}"),
    }
}

#[test]
fn parses_a_claimable_drop_message() {
    let inner = serde_json::json!({
        "type": "drop-claim",
        "data": { "drop_id": "d1", "drop_instance_id": "inst-9" }
    })
    .to_string();
    let frame = serde_json::json!({
        "type": "MESSAGE",
        "data": { "topic": "user-drop-events.999", "message": inner }
    })
    .to_string();

    match parse_pubsub_frame(&frame) {
        Some(PubSubMessage::DropClaimable { drop_id, claim_id }) => {
            assert_eq!(drop_id.0, "d1");
            assert_eq!(claim_id, "inst-9");
        }
        other => panic!("expected a claimable drop, got {other:?}"),
    }
}

#[test]
fn parses_stream_up_and_down() {
    for (kind, expect_up) in [("stream-up", true), ("stream-down", false)] {
        let inner = serde_json::json!({ "type": kind }).to_string();
        let frame = serde_json::json!({
            "type": "MESSAGE",
            "data": { "topic": "video-playback-by-id.12345", "message": inner }
        })
        .to_string();

        match (parse_pubsub_frame(&frame), expect_up) {
            (Some(PubSubMessage::StreamUp { channel_id }), true) => assert_eq!(channel_id.0, "12345"),
            (Some(PubSubMessage::StreamDown { channel_id }), false) => assert_eq!(channel_id.0, "12345"),
            (other, _) => panic!("unexpected {other:?} for {kind}"),
        }
    }
}

#[test]
fn ignores_pong_and_response_frames() {
    assert!(parse_pubsub_frame(r#"{"type":"PONG"}"#).is_none());
    assert!(parse_pubsub_frame(r#"{"type":"RESPONSE","error":""}"#).is_none());
    assert!(parse_pubsub_frame("not json at all").is_none());
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo add tokio-tungstenite --no-default-features --features connect,rustls-tls-webpki-roots -p btdm-twitch
cargo add futures-util -p btdm-twitch
```

Run: `cargo test -p btdm-twitch --test pubsub`
Expected: FAIL — `btdm_twitch::pubsub` does not exist.

- [ ] **Step 3: Implement**

`crates/btdm-twitch/src/pubsub.rs`:

```rust
//! Twitch PubSub.
//!
//! Pushes drop progress, claim availability and stream up/down. The engine also
//! polls as a backstop, because a socket that is connected but silent looks
//! identical to one that is simply quiet.

use crate::TwitchError;
use btdm_core::api::DropProgress;
use btdm_core::model::{ChannelId, DropId};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub const PUBSUB_URL: &str = "wss://pubsub-edge.twitch.tv/v1";
/// Twitch drops the connection if it does not see a PING within ~5 minutes.
pub const PING_INTERVAL_SECS: u64 = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    UserDrops,
    UserPoints,
    StreamState,
}

impl Topic {
    pub fn as_str(&self, target_id: &str) -> String {
        let prefix = match self {
            Topic::UserDrops => "user-drop-events",
            Topic::UserPoints => "community-points-user-v1",
            Topic::StreamState => "video-playback-by-id",
        };
        format!("{prefix}.{target_id}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubSubMessage {
    DropProgress(DropProgress),
    DropClaimable { drop_id: DropId, claim_id: String },
    StreamUp { channel_id: ChannelId },
    StreamDown { channel_id: ChannelId },
    PointsClaimable { channel_id: ChannelId, claim_id: String },
}

/// PubSub wraps a JSON *string* inside a JSON envelope, so this decodes twice.
pub fn parse_pubsub_frame(raw: &str) -> Option<PubSubMessage> {
    let envelope: Value = serde_json::from_str(raw).ok()?;
    if envelope.get("type")?.as_str()? != "MESSAGE" {
        return None;
    }
    let data = envelope.get("data")?;
    let topic = data.get("topic")?.as_str()?;
    let inner: Value = serde_json::from_str(data.get("message")?.as_str()?).ok()?;
    let target_id = topic.split_once('.').map(|(_, id)| id).unwrap_or_default();

    match inner.get("type").and_then(Value::as_str)? {
        "drop-progress" => {
            let d = inner.get("data")?;
            Some(PubSubMessage::DropProgress(DropProgress {
                drop_id: DropId(d.get("drop_id")?.as_str()?.to_owned()),
                current_minutes: d.get("current_progress_min")?.as_u64()? as u32,
                required_minutes: d.get("required_progress_min")?.as_u64()? as u32,
            }))
        }
        "drop-claim" => {
            let d = inner.get("data")?;
            Some(PubSubMessage::DropClaimable {
                drop_id: DropId(d.get("drop_id")?.as_str()?.to_owned()),
                claim_id: d.get("drop_instance_id")?.as_str()?.to_owned(),
            })
        }
        "stream-up" => Some(PubSubMessage::StreamUp { channel_id: ChannelId(target_id.to_owned()) }),
        "stream-down" => Some(PubSubMessage::StreamDown { channel_id: ChannelId(target_id.to_owned()) }),
        "points-earned" | "claim-available" => {
            let claim = inner.get("data")?.get("claim")?;
            Some(PubSubMessage::PointsClaimable {
                channel_id: ChannelId(claim.get("channel_id")?.as_str()?.to_owned()),
                claim_id: claim.get("id")?.as_str()?.to_owned(),
            })
        }
        _ => None,
    }
}

pub struct PubSubClient;

impl PubSubClient {
    /// Connects, subscribes, and streams decoded messages until the socket dies.
    /// Reconnection is the caller's concern — the engine owns that policy.
    pub async fn connect(
        url: &str,
        token: &str,
        topics: Vec<String>,
    ) -> Result<mpsc::Receiver<PubSubMessage>, TwitchError> {
        use tokio_tungstenite::tungstenite::Message;

        let (stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;
        let (mut write, mut read) = stream.split();

        let listen = json!({
            "type": "LISTEN",
            "nonce": "btdm",
            "data": { "topics": topics, "auth_token": token },
        });
        write
            .send(Message::Text(listen.to_string().into()))
            .await
            .map_err(|e| TwitchError::Network(e.to_string()))?;

        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(async move {
            let mut ping =
                tokio::time::interval(std::time::Duration::from_secs(PING_INTERVAL_SECS));
            // The first tick fires immediately; skip it, we just connected.
            ping.tick().await;

            loop {
                tokio::select! {
                    _ = ping.tick() => {
                        if write.send(Message::Text(r#"{"type":"PING"}"#.into())).await.is_err() {
                            break;
                        }
                    }
                    frame = read.next() => {
                        match frame {
                            Some(Ok(Message::Text(text))) => {
                                if let Some(msg) = parse_pubsub_frame(&text)
                                    && tx.send(msg).await.is_err()
                                {
                                    break;
                                }
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Err(e)) => {
                                tracing::warn!(error = %e, "pubsub socket error");
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
            tracing::info!("pubsub connection closed");
        });

        Ok(rx)
    }
}
```

Add `pub mod pubsub;` to `crates/btdm-twitch/src/lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm-twitch --test pubsub`
Expected: PASS — 5 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm-twitch
git commit -m "feat(twitch): PubSub client for drop progress and stream state"
```

---

### Task 14: The `TwitchApi` implementation

Wires transport, operations and parsing into the trait the engine consumes.

**Files:**
- Create: `crates/btdm-twitch/src/api_impl.rs`
- Modify: `crates/btdm-twitch/src/lib.rs`
- Test: `crates/btdm-twitch/tests/api_impl.rs`

**Interfaces:**
- Consumes: everything from Tasks 6–13.
- Produces: `TwitchClient::new(http, token, user_id) -> TwitchClient` implementing `btdm_core::api::TwitchApi`; `TwitchClient::with_endpoints(...)` as the test seam.

- [ ] **Step 1: Write the failing test**

`crates/btdm-twitch/tests/api_impl.rs`:

```rust
use btdm_core::api::TwitchApi;
use btdm_core::model::*;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn fetch_inventory_returns_parsed_campaigns() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "currentUser": { "inventory": { "dropCampaignsInProgress": [{
                    "id": "camp-1",
                    "name": "Camp",
                    "status": "ACTIVE",
                    "startAt": "2026-08-01T00:00:00Z",
                    "endAt": "2026-09-01T00:00:00Z",
                    "game": { "id": "g-1", "name": "Deadlock", "slug": "deadlock" },
                    "self": { "isAccountConnected": true },
                    "allow": { "channels": [] },
                    "timeBasedDrops": [{
                        "id": "drop-1",
                        "name": "Reward",
                        "requiredMinutesWatched": 60,
                        "benefitEdges": [],
                        "self": { "currentMinutesWatched": 10, "isClaimed": false, "dropInstanceID": null }
                    }]
                }]}}
            }
        })))
        .mount(&server)
        .await;

    let client = btdm_twitch::api_impl::TwitchClient::with_endpoints(
        reqwest::Client::new(),
        "token".into(),
        UserId::from("999"),
        format!("{}/gql", server.uri()),
    );

    let campaigns = client.fetch_inventory().await.unwrap();
    assert_eq!(campaigns.len(), 1);
    assert_eq!(campaigns[0].drops[0].current_minutes, 10);
}

#[tokio::test]
async fn claim_drop_sends_the_instance_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "claimDropRewards": { "status": "ELIGIBLE_FOR_ALL" } }
        })))
        .mount(&server)
        .await;

    let client = btdm_twitch::api_impl::TwitchClient::with_endpoints(
        reqwest::Client::new(),
        "token".into(),
        UserId::from("999"),
        format!("{}/gql", server.uri()),
    );
    client.claim_drop("inst-9").await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = reqs[0].body_json().unwrap();
    assert_eq!(body["variables"]["input"]["dropInstanceID"], "inst-9");
}

#[tokio::test]
async fn stream_info_reports_an_offline_channel_as_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "user": { "id": "1", "login": "s", "displayName": "S", "stream": null } }
        })))
        .mount(&server)
        .await;

    let client = btdm_twitch::api_impl::TwitchClient::with_endpoints(
        reqwest::Client::new(),
        "token".into(),
        UserId::from("999"),
        format!("{}/gql", server.uri()),
    );
    assert!(client.stream_info("s").await.unwrap().is_none());
}

#[tokio::test]
async fn available_drops_lists_the_ids_the_channel_can_progress() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "channel": { "viewerDropCampaigns": [
                { "id": "c1", "timeBasedDrops": [{ "id": "d1" }, { "id": "d2" }] }
            ]}}
        })))
        .mount(&server)
        .await;

    let client = btdm_twitch::api_impl::TwitchClient::with_endpoints(
        reqwest::Client::new(),
        "token".into(),
        UserId::from("999"),
        format!("{}/gql", server.uri()),
    );
    let ids = client.available_drops(&ChannelId::from("12345")).await.unwrap();
    assert_eq!(ids, vec![DropId::from("d1"), DropId::from("d2")]);
}

/// An empty list means this channel progresses nothing for this account — the
/// check that stops the miner grinding at an unminable campaign.
#[tokio::test]
async fn available_drops_is_empty_when_the_channel_progresses_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "channel": { "viewerDropCampaigns": [] } }
        })))
        .mount(&server)
        .await;

    let client = btdm_twitch::api_impl::TwitchClient::with_endpoints(
        reqwest::Client::new(),
        "token".into(),
        UserId::from("999"),
        format!("{}/gql", server.uri()),
    );
    assert!(client.available_drops(&ChannelId::from("12345")).await.unwrap().is_empty());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p btdm-twitch --test api_impl`
Expected: FAIL — `btdm_twitch::api_impl` does not exist.

- [ ] **Step 3: Implement**

`crates/btdm-twitch/src/api_impl.rs`:

```rust
//! The concrete `TwitchApi`. Everything above this line is transport; everything
//! below it in the engine is pure logic.

use crate::gql::GqlClient;
use crate::operations as ops;
use crate::parse;
use crate::watch::WatchClient;
use btdm_core::api::{ApiError, DropProgress, TwitchApi};
use btdm_core::model::*;
use serde_json::{Value, json};
use tokio::sync::Mutex;

pub struct TwitchClient {
    gql: GqlClient,
    watch: WatchClient,
    user_id: UserId,
    /// Discovered once, then reused. Cheap to re-derive if it ever goes stale.
    spade_url: Mutex<Option<String>>,
}

impl TwitchClient {
    pub fn new(http: reqwest::Client, token: String, user_id: UserId) -> Self {
        Self {
            gql: GqlClient::new(http.clone(), Some(token)),
            watch: WatchClient::new(http),
            user_id,
            spade_url: Mutex::new(None),
        }
    }

    /// Test seam: point GQL at a mock server.
    pub fn with_endpoints(
        http: reqwest::Client,
        token: String,
        user_id: UserId,
        gql_endpoint: String,
    ) -> Self {
        Self {
            gql: GqlClient::with_endpoint(http.clone(), Some(token), gql_endpoint),
            watch: WatchClient::new(http),
            user_id,
            spade_url: Mutex::new(None),
        }
    }
}

impl TwitchApi for TwitchClient {
    async fn fetch_campaigns(&self) -> Result<Vec<DropCampaign>, ApiError> {
        let data = self
            .gql
            .run(&ops::CAMPAIGNS, json!({ "fetchRewardCampaigns": false }))
            .await?;
        Ok(parse::parse_campaigns(&data)?)
    }

    async fn fetch_inventory(&self) -> Result<Vec<DropCampaign>, ApiError> {
        let data = self
            .gql
            .run(&ops::INVENTORY, json!({ "fetchRewardCampaigns": false }))
            .await?;
        Ok(parse::parse_inventory(&data)?)
    }

    async fn available_drops(&self, channel: &ChannelId) -> Result<Vec<DropId>, ApiError> {
        let data = self
            .gql
            .run(&ops::AVAILABLE_DROPS, json!({ "channelID": channel.0 }))
            .await?;

        let mut ids = Vec::new();
        if let Some(campaigns) = data
            .get("channel")
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
        Ok(ids)
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
        let data = self.gql.run(&ops::STREAM_INFO, json!({ "channel": login })).await?;
        Ok(parse::parse_stream_info(&data)?.map(|(_, stream)| stream))
    }

    async fn send_watch(&self, channel: &Channel, stream: &Stream) -> Result<(), ApiError> {
        let url = {
            let cached = self.spade_url.lock().await.clone();
            match cached {
                Some(url) => url,
                None => {
                    let url = self.watch.discover_spade_url(&channel.login).await?;
                    *self.spade_url.lock().await = Some(url.clone());
                    url
                }
            }
        };
        self.watch.send(&url, channel, stream, &self.user_id).await?;
        Ok(())
    }

    async fn current_progress(&self, channel: &ChannelId) -> Result<Option<DropProgress>, ApiError> {
        let data = self
            .gql
            .run(
                &ops::CURRENT_DROP,
                json!({ "channelID": channel.0, "channelLogin": "" }),
            )
            .await?;

        let Some(session) = data
            .get("currentUser")
            .and_then(|u| u.get("dropCurrentSession"))
            .filter(|v| !v.is_null())
        else {
            return Ok(None);
        };

        let Some(drop_id) = session.get("dropID").and_then(Value::as_str) else {
            return Ok(None);
        };
        Ok(Some(DropProgress {
            drop_id: DropId(drop_id.to_owned()),
            current_minutes: session
                .get("currentMinutesWatched")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            required_minutes: session
                .get("requiredMinutesWatched")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        }))
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
```

Add `pub mod api_impl;` to `crates/btdm-twitch/src/lib.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p btdm-twitch --test api_impl`
Expected: PASS — 5 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/btdm-twitch
git commit -m "feat(twitch): implement TwitchApi over GQL, spade and parsing"
```

---

### Task 15: The mining engine

Ties selection, watching, progress and claiming into one loop. Driven entirely through the `TwitchApi` trait, so every scenario below is tested without a network.

**Files:**
- Create: `crates/btdm-core/src/engine.rs`
- Modify: `crates/btdm-core/src/lib.rs`
- Test: `crates/btdm-core/tests/engine_scenarios.rs`, `crates/btdm-core/tests/support/mod.rs`

**Interfaces:**
- Consumes: `api::TwitchApi`, `eligibility`, `progress::ProgressTracker`, `event::MinerEvent`, `model::*`.
- Produces: `Engine::new(api, config)` where `EngineConfig { watch_interval: Duration, poll_interval: Duration }`; `Engine::sync(&mut self, now) -> Result<(), ApiError>`; `Engine::select_channel(&mut self, now) -> Result<Option<(Channel, Stream, CampaignId)>, ApiError>`; `Engine::tick(&mut self, now) -> Result<(), ApiError>`; `Engine::drain_events(&mut self) -> Vec<MinerEvent>`; `Engine::current_channel(&self) -> Option<&Channel>`.

- [ ] **Step 1: Write the scriptable fake**

`crates/btdm-core/tests/support/mod.rs`:

```rust
//! A scriptable Twitch. Every failure the engine must survive is expressible
//! here, which is the whole reason btdm-core is forbidden from doing I/O.

use btdm_core::api::{ApiError, DropProgress, TwitchApi};
use btdm_core::model::*;
use std::sync::Mutex;

#[derive(Default)]
pub struct FakeTwitch {
    pub inventory: Mutex<Vec<DropCampaign>>,
    pub channels: Mutex<Vec<(Channel, Stream)>>,
    /// Drop IDs each channel can progress. A channel absent from this map
    /// progresses nothing, which is how "unminable" is expressed.
    pub available: Mutex<std::collections::HashMap<String, Vec<DropId>>>,
    pub progress: Mutex<Option<DropProgress>>,
    pub watches_sent: Mutex<Vec<ChannelId>>,
    pub claims: Mutex<Vec<String>>,
    /// When set, every call fails with this error.
    pub fail_with: Mutex<Option<ApiError>>,
}

impl FakeTwitch {
    fn guard(&self) -> Result<(), ApiError> {
        match &*self.fail_with.lock().unwrap() {
            Some(ApiError::Unauthorized) => Err(ApiError::Unauthorized),
            Some(ApiError::RateLimited) => Err(ApiError::RateLimited),
            Some(ApiError::Network(m)) => Err(ApiError::Network(m.clone())),
            Some(ApiError::Malformed(m)) => Err(ApiError::Malformed(m.clone())),
            None => Ok(()),
        }
    }
}

impl TwitchApi for FakeTwitch {
    async fn fetch_campaigns(&self) -> Result<Vec<DropCampaign>, ApiError> {
        self.guard()?;
        Ok(self.inventory.lock().unwrap().clone())
    }
    async fn fetch_inventory(&self) -> Result<Vec<DropCampaign>, ApiError> {
        self.guard()?;
        Ok(self.inventory.lock().unwrap().clone())
    }
    async fn available_drops(&self, channel: &ChannelId) -> Result<Vec<DropId>, ApiError> {
        self.guard()?;
        Ok(self.available.lock().unwrap().get(&channel.0).cloned().unwrap_or_default())
    }
    async fn live_channels(&self, game: &Game, _limit: u32) -> Result<Vec<(Channel, Stream)>, ApiError> {
        self.guard()?;
        Ok(self
            .channels
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.game.as_ref().is_some_and(|g| g.id == game.id))
            .cloned()
            .collect())
    }
    async fn stream_info(&self, login: &str) -> Result<Option<Stream>, ApiError> {
        self.guard()?;
        Ok(self
            .channels
            .lock()
            .unwrap()
            .iter()
            .find(|(c, _)| c.login == login)
            .map(|(_, s)| s.clone()))
    }
    async fn send_watch(&self, channel: &Channel, _stream: &Stream) -> Result<(), ApiError> {
        self.guard()?;
        self.watches_sent.lock().unwrap().push(channel.id.clone());
        Ok(())
    }
    async fn current_progress(&self, _channel: &ChannelId) -> Result<Option<DropProgress>, ApiError> {
        self.guard()?;
        Ok(self.progress.lock().unwrap().clone())
    }
    async fn claim_drop(&self, claim_id: &str) -> Result<(), ApiError> {
        self.guard()?;
        self.claims.lock().unwrap().push(claim_id.to_owned());
        Ok(())
    }
}

pub fn game() -> Game {
    Game { id: GameId::from("g1"), name: "Deadlock".into(), slug: "deadlock".into() }
}

pub fn channel(id: &str, login: &str) -> Channel {
    Channel { id: ChannelId::from(id), login: login.into(), display_name: login.into() }
}

pub fn stream(broadcast: &str) -> Stream {
    Stream {
        broadcast_id: broadcast.into(),
        game: Some(game()),
        title: "t".into(),
        viewers: 100,
        drops_enabled: true,
    }
}

pub fn campaign(id: &str, linked: bool, drops: Vec<TimedDrop>) -> DropCampaign {
    use chrono::TimeZone;
    DropCampaign {
        id: CampaignId::from(id),
        name: format!("Campaign {id}"),
        game: game(),
        starts_at: chrono::Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
        ends_at: chrono::Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
        drops,
        is_account_linked: linked,
        allowed_channels: vec![],
    }
}

pub fn drop(id: &str, required: u32, current: u32) -> TimedDrop {
    TimedDrop {
        id: DropId::from(id),
        name: format!("Drop {id}"),
        benefits: vec![],
        required_minutes: required,
        current_minutes: current,
        claim_id: None,
        is_claimed: false,
        precondition_ids: vec![],
    }
}
```

- [ ] **Step 2: Write the failing scenario tests**

`crates/btdm-core/tests/engine_scenarios.rs`:

```rust
mod support;

use btdm_core::api::DropProgress;
use btdm_core::engine::{Engine, EngineConfig};
use btdm_core::event::{EventKind, RejectReason};
use btdm_core::model::*;
use chrono::{TimeZone, Utc};
use support::*;

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap()
}

fn engine(fake: FakeTwitch) -> Engine<FakeTwitch> {
    Engine::new(fake, EngineConfig::default())
}

#[tokio::test]
async fn selects_an_eligible_channel_and_starts_watching() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1"))];
    fake.available.lock().unwrap().insert("ch1".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    let selected = e.select_channel(now()).await.unwrap();

    assert!(selected.is_some());
    assert_eq!(selected.unwrap().0.login, "streamer1");
}

/// The TDM defect, at engine level: an unlinked campaign is never selected,
/// and the rejection is reported with its reason rather than silently dropped.
#[tokio::test]
async fn never_selects_a_campaign_the_account_is_not_linked_to() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", false, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1"))];
    fake.available.lock().unwrap().insert("ch1".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.select_channel(now()).await.unwrap().is_none());

    let rejected = e.drain_events().into_iter().any(|ev| {
        matches!(ev.kind, EventKind::CampaignRejected { reason: RejectReason::NotLinked, .. })
    });
    assert!(rejected, "the rejection must be reported, not silent");
}

/// A channel Twitch says progresses nothing must not be mined, even when the
/// campaign itself is perfectly eligible.
#[tokio::test]
async fn never_selects_a_channel_that_progresses_nothing() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1"))];
    // Deliberately no entry in `available` for ch1.

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    assert!(e.select_channel(now()).await.unwrap().is_none());
}

#[tokio::test]
async fn a_tick_sends_a_watch_heartbeat() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1"))];
    fake.available.lock().unwrap().insert("ch1".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    assert_eq!(e.api().watches_sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn confirmed_progress_is_reported_but_repeats_are_not() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1"))];
    fake.available.lock().unwrap().insert("ch1".into(), vec![DropId::from("d1")]);
    *fake.progress.lock().unwrap() = Some(DropProgress {
        drop_id: DropId::from("d1"),
        current_minutes: 1,
        required_minutes: 60,
    });

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();

    e.tick(now()).await.unwrap();
    let confirmations = e
        .drain_events()
        .into_iter()
        .filter(|ev| matches!(ev.kind, EventKind::ProgressConfirmed { .. }))
        .count();
    assert_eq!(confirmations, 1);

    // Advance past the poll interval so a poll definitely happens, otherwise
    // this would pass merely because no poll was due.
    e.tick(now() + chrono::Duration::seconds(150)).await.unwrap();
    let confirmations = e
        .drain_events()
        .into_iter()
        .filter(|ev| matches!(ev.kind, EventKind::ProgressConfirmed { .. }))
        .count();
    assert_eq!(confirmations, 0, "a repeated reading is not progress");
}

#[tokio::test]
async fn claims_a_drop_as_soon_as_it_becomes_claimable() {
    let fake = FakeTwitch::default();
    let mut d = drop("d1", 60, 60);
    d.claim_id = Some("inst-1".into());
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![d])];
    *fake.channels.lock().unwrap() = vec![(channel("ch1", "streamer1"), stream("bc-1"))];
    fake.available.lock().unwrap().insert("ch1".into(), vec![DropId::from("d1")]);

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.tick(now()).await.unwrap();

    assert_eq!(*e.api().claims.lock().unwrap(), vec!["inst-1".to_string()]);
    assert!(
        e.drain_events().into_iter().any(|ev| matches!(ev.kind, EventKind::DropClaimed { .. })),
        "claiming must be reported"
    );
}

#[tokio::test]
async fn an_expired_token_surfaces_as_unauthorized_rather_than_a_silent_stop() {
    let fake = FakeTwitch::default();
    *fake.fail_with.lock().unwrap() = Some(btdm_core::api::ApiError::Unauthorized);

    let mut e = engine(fake);
    assert!(matches!(
        e.sync(now()).await,
        Err(btdm_core::api::ApiError::Unauthorized)
    ));
}

#[tokio::test]
async fn switching_channels_clears_progress_from_the_old_one() {
    let fake = FakeTwitch::default();
    *fake.inventory.lock().unwrap() = vec![campaign("c1", true, vec![drop("d1", 60, 0)])];
    *fake.channels.lock().unwrap() = vec![
        (channel("ch1", "streamer1"), stream("bc-1")),
        (channel("ch2", "streamer2"), stream("bc-2")),
    ];
    fake.available.lock().unwrap().insert("ch1".into(), vec![DropId::from("d1")]);
    fake.available.lock().unwrap().insert("ch2".into(), vec![DropId::from("d1")]);
    *fake.progress.lock().unwrap() = Some(DropProgress {
        drop_id: DropId::from("d1"),
        current_minutes: 5,
        required_minutes: 60,
    });

    let mut e = engine(fake);
    e.sync(now()).await.unwrap();
    e.select_channel(now()).await.unwrap();
    e.tick(now()).await.unwrap();
    assert!(e.since_last_confirmation(now()).is_some());

    e.switch_away(now());
    assert_eq!(
        e.since_last_confirmation(now()),
        None,
        "progress on the previous channel says nothing about the next"
    );
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p btdm-core --test engine_scenarios`
Expected: FAIL — `btdm_core::engine` does not exist.

- [ ] **Step 4: Implement the engine**

`crates/btdm-core/src/engine.rs`:

```rust
//! The mining loop.
//!
//! Deliberately generic over `TwitchApi` so the whole thing runs against a
//! scripted fake. Selection here is intentionally simple — first eligible,
//! server-verified channel. Strategies replace `select_channel` in milestone 5;
//! this milestone only has to mine correctly, not cleverly.

use crate::api::{ApiError, TwitchApi};
use crate::eligibility;
use crate::event::{EventKind, MinerEvent, RejectReason};
use crate::model::*;
use crate::progress::ProgressTracker;
use chrono::{DateTime, Duration, Utc};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub watch_interval: Duration,
    pub poll_interval: Duration,
    /// How many channels to consider per game.
    pub channel_limit: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            watch_interval: Duration::seconds(60),
            poll_interval: Duration::seconds(120),
            channel_limit: 30,
        }
    }
}

pub struct Engine<A: TwitchApi> {
    api: A,
    config: EngineConfig,
    campaigns: Vec<DropCampaign>,
    current: Option<(Channel, Stream, CampaignId)>,
    progress: ProgressTracker,
    events: Vec<MinerEvent>,
    last_watch: Option<DateTime<Utc>>,
    last_poll: Option<DateTime<Utc>>,
}

impl<A: TwitchApi> Engine<A> {
    pub fn new(api: A, config: EngineConfig) -> Self {
        Self {
            api,
            config,
            campaigns: Vec::new(),
            current: None,
            progress: ProgressTracker::new(),
            events: Vec::new(),
            last_watch: None,
            last_poll: None,
        }
    }

    pub fn api(&self) -> &A {
        &self.api
    }

    pub fn current_channel(&self) -> Option<&Channel> {
        self.current.as_ref().map(|(c, _, _)| c)
    }

    pub fn drain_events(&mut self) -> Vec<MinerEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn since_last_confirmation(&self, now: DateTime<Utc>) -> Option<Duration> {
        self.progress.since_last_confirmation(now)
    }

    /// Leaves the current channel. Progress there tells us nothing about the next.
    pub fn switch_away(&mut self, _now: DateTime<Utc>) {
        self.current = None;
        self.progress.reset();
        self.last_watch = None;
        self.last_poll = None;
    }

    fn emit(&mut self, at: DateTime<Utc>, kind: EventKind) {
        self.events.push(MinerEvent { at, kind });
    }

    /// Refresh campaigns from Twitch and report which are actually mineable.
    pub async fn sync(&mut self, now: DateTime<Utc>) -> Result<(), ApiError> {
        let campaigns = self.api.fetch_inventory().await?;
        let total = campaigns.len();

        let mut eligible = Vec::new();
        for campaign in campaigns {
            match eligibility::assess(&campaign, now) {
                Ok(()) => eligible.push(campaign),
                Err(reason) => {
                    self.emit(
                        now,
                        EventKind::CampaignRejected { campaign: campaign.id.clone(), reason },
                    );
                }
            }
        }

        self.emit(now, EventKind::CampaignsSynced { total, eligible: eligible.len() });
        self.campaigns = eligible;
        Ok(())
    }

    /// Picks a channel that Twitch confirms can progress one of our drops.
    ///
    /// The `available_drops` check is the load-bearing part: without it we would
    /// happily watch a channel that earns nothing, which is the failure users
    /// experience as a progress bar that never moves.
    pub async fn select_channel(
        &mut self,
        now: DateTime<Utc>,
    ) -> Result<Option<(Channel, Stream, CampaignId)>, ApiError> {
        let campaigns = self.campaigns.clone();

        for campaign in &campaigns {
            let wanted: std::collections::HashSet<DropId> = eligibility::mineable_drops(campaign)
                .into_iter()
                .map(|d| d.id.clone())
                .collect();
            if wanted.is_empty() {
                continue;
            }

            let channels = self.api.live_channels(&campaign.game, self.config.channel_limit).await?;
            for (channel, stream) in channels {
                let available = self.api.available_drops(&channel.id).await?;
                if !available.iter().any(|d| wanted.contains(d)) {
                    continue;
                }

                self.progress.reset();
                self.last_watch = None;
                self.last_poll = None;
                self.current = Some((channel.clone(), stream.clone(), campaign.id.clone()));
                self.emit(
                    now,
                    EventKind::NowWatching { channel: channel.clone(), campaign: campaign.id.clone() },
                );
                return Ok(self.current.clone());
            }

            self.emit(
                now,
                EventKind::CampaignRejected {
                    campaign: campaign.id.clone(),
                    reason: RejectReason::NoEligibleChannel,
                },
            );
        }
        Ok(None)
    }

    /// One iteration: claim anything claimable, send a heartbeat if due, then
    /// pull progress as a backstop to the websocket.
    pub async fn tick(&mut self, now: DateTime<Utc>) -> Result<(), ApiError> {
        self.claim_ready(now).await?;

        let Some((channel, stream, _)) = self.current.clone() else {
            return Ok(());
        };

        let due = self
            .last_watch
            .is_none_or(|last| now - last >= self.config.watch_interval);
        if due {
            match self.api.send_watch(&channel, &stream).await {
                Ok(()) => {
                    self.last_watch = Some(now);
                    self.emit(now, EventKind::WatchSent { channel: channel.id.clone() });
                }
                Err(e) => {
                    self.emit(
                        now,
                        EventKind::WatchFailed { channel: channel.id.clone(), error: e.to_string() },
                    );
                    return Err(e);
                }
            }
        }

        // The websocket is the primary progress source; this poll is only the
        // backstop for a socket that is connected but silent, so it runs on its
        // own slower interval rather than on every heartbeat.
        let poll_due = self
            .last_poll
            .is_none_or(|last| now - last >= self.config.poll_interval);
        if poll_due {
            self.last_poll = Some(now);
            if let Some(p) = self.api.current_progress(&channel.id).await?
                && self.progress.record(p.clone(), now)
            {
                self.emit(
                    now,
                    EventKind::ProgressConfirmed {
                        drop: p.drop_id,
                        current_minutes: p.current_minutes,
                        required_minutes: p.required_minutes,
                    },
                );
            }
        }
        Ok(())
    }

    /// Records confirmed progress arriving over the websocket.
    pub fn on_pushed_progress(
        &mut self,
        p: crate::api::DropProgress,
        now: DateTime<Utc>,
    ) {
        if self.progress.record(p.clone(), now) {
            self.emit(
                now,
                EventKind::ProgressConfirmed {
                    drop: p.drop_id,
                    current_minutes: p.current_minutes,
                    required_minutes: p.required_minutes,
                },
            );
        }
    }

    async fn claim_ready(&mut self, now: DateTime<Utc>) -> Result<(), ApiError> {
        let ready: Vec<(String, DropId, String)> = self
            .campaigns
            .iter()
            .flat_map(|c| &c.drops)
            .filter(|d| d.is_claimable())
            .filter_map(|d| {
                d.claim_id
                    .clone()
                    .map(|cid| (cid, d.id.clone(), d.name.clone()))
            })
            .collect();

        for (claim_id, drop_id, name) in ready {
            self.api.claim_drop(&claim_id).await?;
            for campaign in &mut self.campaigns {
                if let Some(d) = campaign.drops.iter_mut().find(|d| d.id == drop_id) {
                    d.is_claimed = true;
                    d.claim_id = None;
                }
            }
            self.emit(now, EventKind::DropClaimed { drop: drop_id, name });
        }
        Ok(())
    }
}
```

Add `pub mod engine;` to `crates/btdm-core/src/lib.rs`.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p btdm-core --test engine_scenarios`
Expected: PASS — 8 tests.

- [ ] **Step 6: Commit**

```bash
git add crates/btdm-core
git commit -m "feat(core): mining engine with server-verified channel selection

Every scenario, including an unminable channel and an expired token, is
tested against a scripted fake rather than a live account."
```

---

### Task 16: Binary wiring, CLI and logging

Turns the pieces into a runnable daemon.

**Files:**
- Modify: `crates/btdm/src/main.rs`
- Create: `crates/btdm/src/run.rs`
- Test: manual smoke test (documented below)

**Interfaces:**
- Consumes: everything.
- Produces: the `btdm` binary with flags `--data-dir <PATH>`, `--config <PATH>`, `--account <NAME>`, `--headless`, `--log-level <LEVEL>`.

- [ ] **Step 1: Add dependencies**

```bash
cargo add clap --features derive -p btdm
cargo add tracing-subscriber --features env-filter -p btdm
cargo add reqwest --no-default-features --features json,rustls-tls,gzip -p btdm
cargo add anyhow -p btdm
```

- [ ] **Step 2: Write the run loop**

`crates/btdm/src/run.rs`:

```rust
//! Wires config, storage, auth and the engine into a running miner.

use crate::config::Config;
use crate::storage::{Storage, Tokens};
use btdm_core::engine::{Engine, EngineConfig};
use btdm_twitch::api_impl::TwitchClient;
use btdm_twitch::auth::{AuthClient, PollOutcome};
use btdm_twitch::client::build_http_client;
use std::path::Path;
use std::time::Duration;

/// Logs in if needed, then mines until interrupted.
pub async fn run(config: &Config, account: &str, data_dir: &Path) -> anyhow::Result<()> {
    let storage = Storage::open(&data_dir.join("state.db"))?;
    let http = build_http_client()?;

    let tokens = match storage.get_tokens(account)? {
        Some(t) => t,
        None => {
            let t = login(&http, account).await?;
            storage.put_tokens(account, &t)?;
            t
        }
    };

    let auth = AuthClient::new(http.clone());
    let validated = auth.validate(&tokens.access_token).await?;
    tracing::info!(login = %validated.login, "authenticated");

    let api = TwitchClient::new(
        http,
        tokens.access_token.clone(),
        btdm_core::model::UserId(validated.user_id.clone()),
    );

    let engine_config = EngineConfig::default();
    let watch_interval = engine_config.watch_interval.to_std()?;
    let mut engine = Engine::new(api, engine_config);

    let _ = config; // Priority lists are consumed by strategies in milestone 5.

    loop {
        let now = chrono::Utc::now();

        if let Err(e) = engine.sync(now).await {
            tracing::error!(error = %e, "campaign sync failed");
        }
        if engine.current_channel().is_none()
            && let Err(e) = engine.select_channel(now).await
        {
            tracing::error!(error = %e, "channel selection failed");
        }
        if let Err(e) = engine.tick(now).await {
            tracing::error!(error = %e, "mining tick failed");
            // Milestone 4 replaces this with the escalation ladder.
            engine.switch_away(now);
        }

        for event in engine.drain_events() {
            tracing::info!(?event.kind, "event");
            storage.append_event(account, &event)?;
        }

        tokio::time::sleep(watch_interval).await;
    }
}

/// Device-code login. The user enters the code at twitch.tv/activate — which is
/// why this works on a machine with no browser and no display.
async fn login(http: &reqwest::Client, account: &str) -> anyhow::Result<Tokens> {
    let auth = AuthClient::new(http.clone());
    let dc = auth.start_device_flow().await?;

    tracing::warn!(
        account,
        code = %dc.user_code,
        url = %dc.verification_uri,
        "ACTION REQUIRED: open the URL and enter the code"
    );
    println!("\n  Go to {}\n  Enter code: {}\n", dc.verification_uri, dc.user_code);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(dc.expires_in_secs);
    loop {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("device code expired before it was entered");
        }
        tokio::time::sleep(Duration::from_secs(dc.interval_secs)).await;

        if let PollOutcome::Granted { access_token, refresh_token } = auth.poll_once(&dc).await? {
            let v = auth.validate(&access_token).await?;
            return Ok(Tokens {
                access_token,
                refresh_token,
                user_id: v.user_id,
                login: v.login,
            });
        }
    }
}
```

- [ ] **Step 3: Write main**

`crates/btdm/src/main.rs`:

```rust
mod config;
mod run;
mod storage;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "btdm", about = "A Twitch drops miner that tells you the truth")]
struct Args {
    /// Where state.db lives. Defaults to $XDG_DATA_HOME/btdm.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// Config file. Defaults to <data-dir>/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Which configured account to mine with.
    #[arg(long, default_value = "default")]
    account: String,

    /// Run without the web dashboard. (The dashboard arrives in a later milestone;
    /// today this flag only documents the intent.)
    #[arg(long)]
    headless: bool,

    #[arg(long, default_value = "info")]
    log_level: String,
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("btdm")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| args.log_level.clone().into()),
        )
        .init();

    let data_dir = args.data_dir.unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)?;

    let config_path = args.config.unwrap_or_else(|| data_dir.join("config.toml"));
    let config = config::Config::load(&config_path)?;
    // Materialise defaults on first run so the file is there to edit.
    if !config_path.exists() {
        config.save_atomic(&config_path)?;
        tracing::info!(path = %config_path.display(), "wrote a default config");
    }

    tracing::info!(account = %args.account, headless = args.headless, "starting");
    run::run(&config, &args.account, &data_dir).await
}
```

- [ ] **Step 4: Verify it builds and the whole suite passes**

Run: `cargo build --workspace --release`
Expected: succeeds.

Run: `cargo test --workspace`
Expected: PASS — every test from Tasks 1–15.

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Manual smoke test against a real account**

This is the first contact with the live API, so expect to fix parsing details here. Work through it in order:

1. `cargo run -- --data-dir ./devdata --log-level debug`
2. Confirm a code and URL are printed. Enter the code at the printed URL.
3. Confirm the log reports `authenticated` with your login.
4. Confirm `CampaignsSynced` appears with a plausible total. **If `eligible` is 0 but you know you have linked campaigns, the parsing in Task 9 is wrong** — capture the raw response and fix the fixture before continuing.
5. Confirm `NowWatching` names a channel actually streaming a game you have a campaign for.
6. Wait 3–5 minutes. Confirm `ProgressConfirmed` appears with a rising minute count. **This is the acceptance criterion for the whole plan** — if watches send but progress never confirms, the spade payload in Task 11 is wrong.
7. Cross-check the minute count against the Twitch drops inventory page in a browser.

Record anything that needed fixing; those become the first tasks of the milestone-4 plan.

- [ ] **Step 6: Commit**

```bash
git add crates/btdm
git commit -m "feat(btdm): CLI, device-code login and the mining run loop"
```

---

## Definition of done

- `cargo test --workspace` passes; `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- `cargo build --release` produces a single binary that runs with no runtime dependencies.
- The manual smoke test reaches step 6: server-confirmed progress rising against a real account.
- `btdm-core` still has no network dependency (Task 1's guard test proves it).

## Explicitly not in this plan

Deferred to later plans, in this order: the watchdog and escalation ladder; feasibility maths and the five strategies; multi-account; the web dashboard; channel points; Discord alerting; packaging; self-update.
