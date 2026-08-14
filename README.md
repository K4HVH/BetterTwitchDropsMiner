# Better Twitch Drops Miner

A Twitch drops miner that tells you the truth about what it is doing.

Existing miners fail in ways you cannot see: the progress bar advances, the log
looks healthy, and nothing is earned. This one only ever displays progress
Twitch has confirmed, treats a stall as an event rather than a mystery, and
works a defined recovery ladder before giving up.

Single static binary. Runs headless on a Raspberry Pi. Multi-account.

## What makes it different

**Progress is server-confirmed, never a local timer.** The reference
implementation's own README admits its countdown "does not represent nor affect
actual mining speed". Here, if Twitch has not confirmed a minute, nothing moves
— and the dashboard shows how long it has been since the last confirmation, so
a stall is the most obvious thing on screen.

**Eligibility is verified, not assumed.** A campaign your account is not linked
to is never mined, regardless of any setting. This is a real defect in the tool
this replaces: enabling its badges/emotes option skips the account-link check
entirely, so it grinds at campaigns that can never progress.

**Feasibility is computed.** A campaign needing four hours with three hours left
is impossible; it is dropped with a reason instead of consuming the watch slot.
Tiers locked behind unclaimed ones count towards the total, so a one-hour drop
gating a five-hour one is judged on six hours, not one.

**One channel, several campaigns.** Twitch runs multiple campaigns for the same
game, and an hour of watching counts for all of them at once. Every strategy
scores a channel on everything it advances, so a stream serving four campaigns
beats one serving a single campaign — and the dashboard says which.

**One account failing does not stop the others.** A revoked token used to end
the whole process — every other account's miner and the dashboard with it. Now
the account stops, says why, and offers a sign-in link; everything else keeps
mining. You can link a new account or re-authorise a dead one from the
dashboard without restarting anything.

**The dashboard is a control surface, not a status page.** Strategy, pause,
pin, favourites, priority games, per-campaign and per-game exclusions, switch
channel, watch a specific channel, resync — all live, no restart, written back
to `config.toml`. Everything on it is also on the CLI, because a headless box
should not need a browser.

**Recovery is a ladder, not a restart.** On a confirmed stall: resend the
heartbeat, re-fetch stream info (a restarted broadcast earns nothing while
looking healthy), re-verify eligibility, switch channel, re-sync campaigns,
validate auth, quarantine the campaign. Every rung is logged.

**Stale API hashes degrade, not break.** Twitch's private GraphQL uses persisted
query hashes that rot without notice. Every operation also carries its raw query
text, so a stale hash becomes a logged warning and an automatic retry.

## Quick start

```sh
cargo build --release
./target/release/btdm login      # shows a code to enter at twitch.tv/activate
./target/release/btdm mine       # mines, with the dashboard on 127.0.0.1:8080
```

There is no JavaScript toolchain in the build. `cargo build` produces the
finished binary, dashboard included.

### Commands

| Command | Purpose |
|---|---|
| `btdm login [--force]` | Link an account via the device-code flow (also possible from the dashboard) |
| `btdm status [--json]` | What the running miner is doing |
| `btdm accounts` | Which accounts are linked, and whether their tokens still work |
| `btdm logout` | Remove a stored token |
| `btdm mine [--headless]` | Mine every linked account |
| `btdm probe` | One diagnostic pass, then exit |
| `btdm control <cmd>` | Drive a running miner — see below |
| `btdm update [--apply]` | Check for a newer release; install only with `--apply` |

### Controlling a running miner

Every control on the dashboard is on the CLI, over the same API, taking effect
immediately:

`btdm control` is a *client* of a running `btdm mine`, not a second way of
mining: each command reaches the daemon over the same HTTP API the dashboard
uses, and the reply is the miner's own confirmation once it has applied it.

```sh
btdm control pause | resume
btdm control strategy coverage
btdm control watch somestreamer        # switch to a channel now
btdm control switch                    # leave the current one and re-choose
btdm control sync                      # re-fetch campaigns now
btdm control favourite somestreamer [--remove]
btdm control pin somestreamer [--clear]
btdm control priority "Escape from Tarkov" "Rust"   # set the whole list
btdm control priority --add "Rust" | --remove "Rust"
btdm control priority --up "Rust" | --down "Rust"   # reorder one place
btdm control priority --clear
btdm control exclude-game "Some Game" [--undo]
btdm control exclude-campaign <id> [--undo]
btdm control clear-penalties           # forget every channel and campaign penalty
```

Priority entries match a game name *or* a campaign name, so two campaigns for
the same game can be ranked against each other.

Add `--account <name>` to any command to manage a second account, and
`--only-this-account` to `mine` to run just that one.

Updates are never automatic. `btdm update` only reports what is available;
`--apply` downloads it, verifies the published SHA-256 before touching anything,
and swaps the binary by rename. A release without a checksum is refused.

## Configuration

`config.toml` lives in the data directory (`$XDG_DATA_HOME/btdm` by default,
override with `--data-dir`). It is the source of truth for settings; runtime
state lives in `state.db` beside it. Both are created mode `0600`.

```toml
bind = "127.0.0.1:8080"
# proxy = "http://user:pass@host:port"   # all accounts, unless overridden

# Alerts. Any combination; several accounts can also override these per account.
[notify]
# discord = "https://discord.com/api/webhooks/..."
# ntfy    = "https://ntfy.sh/some-topic-only-you-know"   # no account, reaches a phone
# webhook = "https://example.com/hook"                   # JSON POST, carries the raw event

[[accounts]]
name = "default"
strategy = "deadline"
priority_games = ["Deadlock", "Sea of Thieves"]
favourite_channels = ["somestreamer"]
# pinned_channel = "somestreamer"
# excluded_games = ["Some Game I Own Everything For"]
# proxy = "http://..."                   # this account only
# [accounts.notify]                      # this account's own alerts
# ntfy = "https://ntfy.sh/just-this-account"

# Everything below has a working default. Tune it for a slow connection or a
# very small box; values that would stop mining outright are clamped.
[timing]
watch_interval_secs = 60
poll_interval_secs = 120
sync_interval_mins = 10
reselect_interval_mins = 10
channels_per_game = 30
max_channels_per_campaign = 8
availability_checks = 160
check_concurrency = 8
suspect_after_secs = 150
stalled_after_secs = 360
recovery_cooldown_secs = 60
# Channels followed for stream state over the websocket. One is the channel
# being watched, which is the only subscription that clearly pays for itself.
followed_channels = 1
# How often to reconsider while a campaign is close to being lost.
urgent_reselect_mins = 2
```

Anything the dashboard or `btdm control` changes is written back here, so
settings survive a restart.

### Strategies

| Strategy | Behaviour |
|---|---|
| `deadline` *(default)* | Earliest deadline first over campaigns that can still be finished. Nothing achievable is lost to expiry. |
| `availability` | Least headroom first: time left measured against work left. Better than `deadline` across a mix of short and long campaigns. |
| `priority` | `priority_games` first, top down, then everything else by urgency. Never sits idle. |
| `priority-only` | Only the listed games, even if that means mining nothing. |
| `coverage` | The channel advancing the most campaigns at once. |
| `completionist` | Finish the campaign closest to done before starting another. |
| `manual` | Watch `pinned_channel` and nothing else. |

The choice is reconsidered periodically rather than only when idle, so a
campaign that becomes urgent can take over — with hysteresis, so it does not
thrash between two near-equal channels.

A `priority_games` entry matches a game by name, or a campaign by name as a
substring — so `"Ignite"` covers "Ignite MSF 2026 Day 1" through "Day 4"
without listing each, and two campaigns for the same game can be ranked
against each other.

`favourite_channels` is not a strategy — it is a tie-break applied under all of
them, so when two channels are equally good for drops the one you care about
wins, and your channel points accumulate there.

## Deployment

**Docker**

```sh
docker build -t btdm .
docker run -d --name btdm -p 8080:8080 -v btdm-data:/data btdm
docker exec -it btdm btdm --data-dir /data login
```

**systemd** — copy `packaging/btdm.service` to `~/.config/systemd/user/`, then:

```sh
systemctl --user enable --now btdm
journalctl --user -u btdm -f
```

## Troubleshooting

**Nothing progresses, and the log says the watch endpoint is unreachable.**
Allow-list `spade.twitch.tv`. It is an analytics domain, so ad-blockers, Pi-hole
and VPN filters commonly block it — and it is the *only* endpoint that advances
drop time. Every miner depends on it; most fail silently when it is blocked.
This one says so.

**A campaign says your account is not linked.** The dashboard's Unavailable tab
links straight to the right page for each one. The miner will never mine an
unlinked campaign, because doing so cannot earn anything.

**A campaign says there is not enough time left.** It cannot be finished before
it expires. This is a statement about the clock, not a bug.

**Is it actually working?** `GET /api/health` returns 503 when any account has
stalled, and carries no account names, channels or campaigns — it is the one
unauthenticated route, and the Docker image uses it as its healthcheck.

## Security

`state.db` holds live Twitch OAuth tokens in plaintext, mode `0600`. Treat it
like a password file. There is no keyring on a headless Pi, so this is a
deliberate, documented trade-off rather than an oversight.

The dashboard binds to loopback by default. Binding anywhere else generates a
bearer token from OS entropy, stores it in `config.toml` so a restart does not
invalidate your bookmark, and requires it on every route except `/api/health`.

## Architecture

Four crates. The load-bearing constraint is that **`btdm-core` cannot make a
network call** — it talks to a `TwitchApi` trait, so the mining engine, the
strategies and the watchdog are all tested against synthetic worlds rather than
a live account. A test that fails the build if a networking dependency enters
`btdm-core` enforces this.

| Crate | Responsibility |
|---|---|
| `btdm-core` | Domain model, eligibility, feasibility, strategies, watchdog, engine |
| `btdm-twitch` | Auth, GraphQL transport, spade heartbeat, PubSub |
| `btdm-web` | Dashboard: axum, SSE, embedded assets |
| `btdm` | Config, storage, CLI, supervisor |

The dashboard is a single hand-written HTML file with no build step. The cost
is that `cargo test` cannot prove it renders, so `tools/render-check` loads it
into jsdom against a real snapshot and drives every tab and control. Run it by
hand when changing the page.

Design and implementation notes live in `docs/superpowers/`.

## Licence

MIT.
