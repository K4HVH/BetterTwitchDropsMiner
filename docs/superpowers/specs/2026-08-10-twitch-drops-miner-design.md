# BetterTwitchDropsMiner — Design

**Date:** 2026-08-10
**Status:** Approved for planning

## Problem

Existing Twitch drop miners fail in ways their users cannot see or diagnose. The
reference implementation (DevilXD/TwitchDropsMiner, "TDM") states in its own README that
it is not intended for unattended operation and that "fatal errors are expected every so
often". Its dominant failure mode is silent non-progress: the UI reports mining, the
progress bar advances, and nothing is actually earned.

Three concrete causes are visible in TDM's source and issue tracker:

1. **The progress bar is a local approximation**, not server-confirmed state. It advances
   whether or not Twitch is counting the time.
2. **Eligibility is inferred from a settings flag rather than verified.** In
   `inventory.py:396-400`, enabling the badges/emotes option makes any campaign containing
   a badge or emote eligible *without checking the account is linked to it*, so the miner
   grinds at campaigns that can never progress.
3. **No feasibility analysis.** Campaigns that cannot possibly complete before their
   deadline are mined anyway, consuming the single watch slot.

Structurally, TDM is a single-account desktop application: `twitch.py` is 1,642 lines,
`gui.py` is 2,971 lines, and the mining logic cannot be tested without a live Twitch
account. Its failure paths are therefore the least-tested part of the system, which is
where its bugs live.

## Goals

Build a Rust replacement that is:

- **Honest** — displays only server-confirmed progress; makes stalls visible and loud.
- **Self-healing** — detects stalls and works a defined escalation ladder before alerting.
- **Smart** — treats channel selection as a deadline-scheduling problem with pluggable
  strategies, not a fixed priority list.
- **Headless-capable** — runs unattended on a Raspberry Pi with no display, managed
  through a web dashboard.
- **Multi-account** — several independent miners under one dashboard.
- **Testable** — the mining engine can be tested against adversarial scenarios offline.

## Non-goals for v1

Explicitly deferred, with the architecture leaving room for each:

- Automatic A/B testing or self-tuning between strategies. Strategies are user-selected.
- Concurrent watching of multiple channels on one account.
- Platforms other than Twitch.
- Proxy support. The HTTP client is account-scoped, so this is later a config line.
- Localisation.

---

## Architecture

A Cargo workspace producing a single static binary.

| Crate | Responsibility | May depend on |
|---|---|---|
| `btdm-twitch` | All Twitch communication: device-code auth, GQL transport, PubSub websocket, response parsing | reqwest, tokio-tungstenite |
| `btdm-core` | Domain model and mining engine: campaigns, drops, eligibility, feasibility, strategies, watchdog | **no network crates** |
| `btdm-web` | axum server, embedded assets, SSE, control endpoints | `btdm-core` |
| `btdm` | Binary: config, wiring, CLI, daemon mode | all of the above |

**The load-bearing constraint is that `btdm-core` cannot make a network call.** It depends
on a `TwitchApi` trait that `btdm-twitch` implements. This is what makes the failure paths
testable: "the stream restarts, progress stalls for six minutes, and the token expires at
the same moment" becomes an ordinary unit test rather than something you can only observe
in production.

### Concurrency

One tokio task per account, owning its state exclusively. Accounts share no mutable state,
so their independence is enforced by the type system rather than by convention. Each task
emits `MinerEvent` values onto a broadcast channel.

Every other subsystem is a consumer of that event stream: the web UI via SSE, the Discord
notifier, and the SQLite event log.

```
┌─ account task ─┐   ┌─ account task ─┐
│ auth·sync·sched│   │ auth·sync·sched│
│ watch·watchdog │   │ watch·watchdog │
└───────┬────────┘   └───────┬────────┘
        └──────────┬─────────┘
              MinerEvent bus
        ┌──────────┼──────────┐
     web/SSE   Discord    SQLite log
```

`--headless` simply does not start the web server. There is no second code path, so the
daemon cannot behave differently from the interactive build.

---

## How mining works

Established by reading TDM and two other implementations; all converge on this mechanism.

1. **Auth** — OAuth 2.0 device-code flow against `id.twitch.tv/oauth2/device`. The user
   visits `twitch.tv/activate` and enters a code. No browser automation, no cookie jar.
   Token validated periodically against `id.twitch.tv/oauth2/validate`.
2. **Discovery** — GQL persisted queries fetch available campaigns and the user's
   inventory, and resolve games to live channels.
3. **Watching** — a synthetic `minute-watched` event is posted to Twitch's "spade"
   endpoint roughly every 59 seconds. No video or audio is ever fetched.
4. **Progress** — reported back by Twitch via PubSub websocket events and the
   `DropCurrentSessionContext` query.
5. **Claiming** — a GQL mutation against the drop instance ID.

### GQL resilience

TDM hardcodes persisted-query SHA-256 hashes; several of its commits are nothing but hash
updates. When a hash goes stale, behaviour degrades quietly.

Each operation here is defined as `{ name, sha256, raw_query }`. The client sends the
persisted hash first; on a `PersistedQueryNotFound` error it immediately retries the same
request with the raw query text, logs a warning, and surfaces "stale hash" in a
diagnostics panel. Unpersisted queries are known to be accepted — TDM's own spade mutation
sends raw query text today. Each operation's raw-query fallback is verified during
implementation.

A stale hash therefore becomes a logged warning rather than a silent failure.

---

## Reliability core

### Verification

Progress is only ever displayed from server-confirmed sources, never a local timer:

- **Push:** PubSub `user-drop-events` progress messages.
- **Pull:** `DropCurrentSessionContext`, polled as a backstop for when the socket is alive
  but silent.

The UI displays `confirmed 23/60 min · last confirmed 41s ago`. When Twitch stops
confirming, the second figure visibly climbs, making the previously invisible failure the
most obvious thing on screen.

### Watchdog

```
Establishing ──first confirmation──▶ Mining ──no confirmation in grace──▶ Suspect
                                       ▲                                     │
                                       └──────confirmation arrives───────────┤
                                                                             ▼
                                                                          Stalled
                                                                        (escalate)
```

On reaching `Stalled`, the escalation ladder runs cheapest-first. Every rung emits an
event, so the recovery is visible in the UI and the log:

1. Re-send the watch heartbeat.
2. Re-fetch stream info. The broadcast ID changes when a stream restarts, and a stale one
   earns nothing while appearing healthy.
3. Re-verify channel eligibility via `DropsHighlightService_AvailableDrops`.
4. Switch channel; quarantine the current one with exponential backoff.
5. Re-sync campaigns — the campaign may have ended mid-session.
6. Validate and refresh auth.
7. Quarantine the campaign and alert via Discord.

Rung 3 is where the badges/emotes class of bug is caught by construction.

### Timings

Starting values, all configurable. Chosen to match observed client behaviour rather than
to mine as fast as possible.

| Interval | Value | Rationale |
|---|---|---|
| Watch heartbeat | 60s | Matches TDM's proven 59s cadence. |
| Progress poll backstop | 120s | Catches a live-but-silent websocket. |
| `Mining` → `Suspect` | 150s without confirmation | Twitch reports irregularly; two missed heartbeats is suspicious, not yet wrong. |
| `Suspect` → `Stalled` | 360s without confirmation | Six minutes of nothing is a real failure, not jitter. |
| Channel online settle | 120s | A channel that just came up may not have drops active yet. |
| Campaign/inventory resync | 30 min, plus on relevant events | |
| Auth token validation | 60 min | |

### Decision pipeline

Re-evaluated on world change. Only stage 4 varies by strategy.

1. **Refresh** the world snapshot.
2. **Eligibility filter** — server-verified. Never inferred from a settings flag.
3. **Feasibility filter** — `slack = time_to_deadline − minutes_still_needed`, computed
   **per drop tier**. Campaigns are tiered (1h → item A, 3h → item B); tier B can be
   hopeless while tier A remains winnable. Negative slack excludes the tier and marks it
   *impossible* with a reason, rather than grinding at it indefinitely.
4. **Strategy scores candidates** (see below).
5. **Tie-break** — favourite-channel bonus, then campaign coverage, then stream stability.
6. **Hysteresis** — switch only if the challenger beats the incumbent by a margin, so the
   miner does not thrash between two near-equal channels.

Stage 3 yields an **"at risk"** view for free: achievable campaigns ranked by how close
they are to being lost.

---

## Strategies

Pluggable and user-selected per account:

```rust
trait MiningStrategy {
    /// Given the current world state, what should this account do right now?
    fn decide(&self, world: &WorldView) -> Decision;
}
```

`WorldView` carries every known campaign with its drops, tiers, deadlines and confirmed
progress, the set of live channels, and verified eligibility facts. `Decision` is watch a
channel / switch / idle / quarantine.

Because `btdm-core` is I/O-free, a strategy is a pure function over a snapshot. Strategies
are unit-testable against synthetic worlds and comparable against one another on replayed
history without touching Twitch.

**Shipped in v1:**

| Strategy | Behaviour |
|---|---|
| **Deadline (default)** | Earliest-deadline-first over feasible tiers: mine the least-slack thing still achievable. Nothing achievable is lost to expiry. |
| **Priority list** | Strict top-down ordering of chosen games. TDM-compatible, predictable escape hatch. |
| **Coverage** | Prefer the channel advancing the most campaigns at once. Parallelism from a single watch slot. |
| **Completionist** | Lock onto one campaign and finish it before moving on, minimising half-done campaigns. |
| **Manual / pinned** | Mine a pinned channel or campaign, ignoring the scheduler. |

**Favourite channels** are a cross-cutting preference, not a strategy. They apply as a
tie-break bonus under *every* strategy: when two channels are equally good for drops, the
favoured one wins, so channel points accumulate where the user wants them.

---

## Mining scope

- **Timed drops** — the core mechanism.
- **Badge and emote campaigns** — same mechanism, but gated on verified eligibility so an
  unminable campaign is never selected.
- **Channel points** — auto-claim the bonus chest on the channel currently being watched,
  via PubSub `community-points-user-v1` and the `ClaimCommunityPoints` mutation.

---

## Interfaces

### Web dashboard

axum with assets embedded via `rust-embed`. Vanilla JavaScript and server-sent events,
with **no JavaScript build step**, so `cargo build` alone produces the finished binary.

Views: accounts overview, campaigns (including *at risk* and *impossible* with reasons),
channels, inventory, settings, and the event log. SSE pushes live state; control actions
are ordinary POSTs.

Device-code login is presented in the dashboard: the code is displayed for the user to
enter at `twitch.tv/activate` from any device.

**Security.** Binds `127.0.0.1:8080` by default. Binding to any other interface
auto-generates a bearer token, printed once at startup and required thereafter. This
matters because the state database holds live Twitch OAuth tokens.

### Configuration and storage

- **`config.toml`** is the source of truth for settings: accounts, strategy per account,
  priority lists, favourites, webhook URL, bind address. The UI rewrites it atomically
  (temp file, fsync, rename), keeping Docker and systemd deployments declarative.
- **`state.db`** — SQLite via `rusqlite` with the `bundled` feature, so there is no system
  dependency and the binary stays static. Holds auth tokens, campaign cache, claimed-drop
  history, a capped event log, and progress samples.

Both are created with `0600` permissions. Tokens are stored in plaintext; this is
documented prominently, since a headless Pi has no keyring to use instead. OS keyring
support may follow.

Data directory resolves to `$XDG_DATA_HOME/btdm`, or `./data` under Docker, overridable
with `--data-dir`.

### Alerting

- The dashboard event log, always.
- An optional Discord webhook for drop claimed, stall detected, escalation exhausted, and
  auth expired.

---

## Testing

The reliability claim is only credible if the failure paths are tested.

- **`btdm-core`** — pure unit tests: strategy scoring, feasibility and tier maths,
  watchdog state transitions, eligibility rules, hysteresis.
- **`btdm-twitch`** — fixture tests over recorded real GQL responses, so parsing
  regressions are caught offline. A separate opt-in live-contract test hits the real API
  and validates response shapes, run deliberately rather than on every build, so schema
  drift is detected on purpose.
- **Integration** — a `FakeTwitchApi` scripting adversarial scenarios (stream restart,
  silent socket, token expiry, campaign ending mid-session, permanently unminable
  campaign) asserting that the engine recovers and that recovery is observable in the
  event stream.

---

## Packaging

- Static binaries for `x86_64` and `aarch64`, using rustls rather than OpenSSL to avoid
  system dependencies.
- A Docker image with a data volume.
- A systemd unit plus `--daemon` mode.
- Opt-in self-update against GitHub releases with checksum verification. Built last, as it
  is the feature most able to break an unattended machine.

---

## Delivery order

v1 is large, so it is built in milestones that each end somewhere useful. Milestone 3 is
the first point at which the tool earns drops; everything after it is reliability, reach,
and polish.

1. **Foundations** — workspace, config, SQLite storage, device-code auth, GQL transport
   with the raw-query fallback.
2. **Discovery** — campaign and inventory model, verified eligibility, fixture tests.
3. **Mining loop** — watch heartbeat, PubSub, server-confirmed verification, claiming.
   Single account, headless, log only. *It mines.*
4. **Reliability** — watchdog, escalation ladder, adversarial integration scenarios.
5. **Strategies** — feasibility and tier maths, deadline default, the remaining
   strategies, favourites, hysteresis.
6. **Multi-account.**
7. **Web dashboard** — axum, SSE, all views, bearer-token guard.
8. **Channel points.**
9. **Discord alerting.**
10. **Packaging** — static x86_64/aarch64 builds, Docker image, systemd unit.
11. **Self-update.**

## Risks

- **Twitch changes the watch mechanism.** Mitigated by isolating it behind a single trait
  in `btdm-twitch`, so a replacement is one implementation rather than a refactor.
- **Persisted-query hashes go stale.** Mitigated by the raw-query fallback.
- **Response schemas drift.** Mitigated by fixture tests plus the deliberate live-contract
  test.
- **Plaintext token storage.** Accepted for v1 and documented; no keyring exists on the
  target headless deployment.
- **Account flagging from multi-account use on one IP.** Accepted for now at the user's
  direction; the account-scoped HTTP client leaves room for per-account proxies later.
