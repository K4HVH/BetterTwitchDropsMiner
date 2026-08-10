# Status

What is built, what has been verified against a live account, and what is
still missing. Kept honest deliberately — a status file that only lists wins
is worth nothing.

## Verified against a live account

Account `k4hvh` (530826710), repeatedly, over the course of the work:

- Device-code login, token validation, `expires_in: 0` handled as "never
  expires" rather than "expiring now"
- 86 campaigns discovered, 16 mineable, 69 rejected as unlinked with a working
  link to each one's own linking page
- Spade heartbeat accepted (204), **server-confirmed progress climbing**, a
  drop reaching 60/60 and the next tier taking over
- Campaign switching, the feasibility filter abandoning an impossible campaign
  unprompted, the recovery ladder firing at exactly 360s on a non-earning
  channel
- The dashboard serving snapshots, pushing them over SSE, and applying commands
  — strategy changed from the CLI took effect in under a second and was written
  back to `config.toml`
- An allow-listed campaign restricted to a single small streamer went from
  invisible to mineable once allow-lists were resolved by name
- A selection pass went from 27 seconds (60 sequential availability checks,
  136 channels never looked at) to under 15 with every channel covered

## Defects found and fixed

The ones worth remembering, because each was invisible until something specific
was tried.

**From live running.** `spade.twitch.tv` DNS-blackholed by the user's own
adblocker — the single endpoint that advances drop time, which every miner
fails silently on. Twitch returning `displayName` and no slug from the campaign
dashboard, which discarded 129 of 130 campaigns. A status line showing one
drop's minutes while a different one was confirming.

**From adversarial review of the code.** A refused claim reported as a success
and marked claimed locally, never retried. Progress accepted account-wide, so
browsing Twitch in a browser reset the miner's stall clock — twice, at
different scopes. A flaky directory request silently swallowing a user command
for ten minutes. Excluding one campaign tearing down a session advancing
several. PubSub reconnection done inline, blocking heartbeats and Ctrl-C. The
dashboard rebuilding itself over half-typed input every heartbeat.

**From comparing features against the reference implementation.** Feasibility
computed over unlocked tiers only, so a 1h-then-5h campaign reported it needed
one hour — the flagship number, wrong for every tiered campaign. Drops with no
`dropInstanceID` never claimed at all. Campaigns discarded the instant they
expired, inside Twitch's 24-hour claim window. `priority` refusing to mine
anything unlisted and sitting idle. The stream-state PubSub topic defined,
parsed and handled but never subscribed. The event log written to SQLite as a
Rust debug string that nothing could read back. The release workflow uploading
90-day CI artifacts and never publishing a release, so `btdm update` could
never find anything — a headline feature that had never once worked.

**From running it.** Subscribing to 45 stream-state topics in one LISTEN frame
made Twitch close the socket with WebSocket code 1009, "message too big",
about a second later — a reconnect loop that subscribed, died and subscribed
again every two seconds while the poll backstop quietly carried mining. No
test would have caught it; the connection tolerates 50 topics, it just will
not accept them at once. Frames are chunked at 20 now, and a rejected
subscription is logged rather than silently delivering nothing.

**Security.** An OAuth token committed to local git history (never pushed;
history rewritten, verified absent from every object). The dashboard bearer
token generated from `RandomState::hash_one`, which is seeded for hash-flooding
resistance rather than secrecy.

## Not built

Honest list of what a serious miner could do that this does not.

- **A large channel pool, deliberately.** The reference implementation
  subscribes to hundreds of channels' stream state because it rebuilds its
  channel list only once an hour and has no other liveness signal in between.
  This miner re-queries the directory every ten minutes while mining and every
  sixty seconds while idle, and that query returns only live channels — so the
  extra subscriptions are largely redundant here. One is followed: the channel
  being watched, without which a stream ending goes unnoticed for six minutes.
  Deadline pressure, the case where reacting sooner genuinely matters, is
  handled by reconsidering every two minutes while anything is at risk.
  `followed_channels` raises the pool for anyone who wants it; the mechanism
  handles up to what one connection carries.
- **Per-campaign priority and per-drop targeting.** Priority and exclusion are
  keyed on games and whole campaigns; two campaigns for the same game cannot be
  ranked against each other, and "get me tier 1 and stop" cannot be expressed.
- **`gameEventDrops` cross-referencing.** It is fetched and discarded. The
  reference implementation uses it to infer claims for campaigns whose detail
  response carries no `self` edge.
- **Per-drop time windows.** A tier that only opens in week three of a campaign
  is treated as available immediately.
- **Scheduled wake-ups for upcoming campaigns.** Their start dates are shown,
  but nothing acts on them; a campaign starting in ten minutes is picked up at
  the next sync rather than at its start.
- **Translations.** English only.
- **A keyring.** Tokens are plaintext at mode 0600, documented as a deliberate
  trade-off for headless boxes.

## Test coverage, and where it is thin

381 tests, no clippy warnings.

Well covered: the engine against a scripted Twitch (93 scenarios), strategies,
eligibility and feasibility maths, the watchdog ladder, parsing, the control
plane end to end over HTTP, retry behaviour against a real socket, and the
dashboard rendered in jsdom against a real snapshot.

Thin: `run.rs`'s outer loop sequencing and the PubSub supervisor are still
verified by running the binary rather than by tests — the dynamic
subscription's plumbing is unit-tested, but nothing proves the supervisor
re-subscribes correctly after a reconnect except watching it. The JSON fixtures
are hand-built from observed shapes rather than captured responses.
