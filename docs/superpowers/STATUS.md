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

**Security.** An OAuth token committed to local git history (never pushed;
history rewritten, verified absent from every object). The dashboard bearer
token generated from `RandomState::hash_one`, which is seeded for hash-flooding
resistance rather than secrecy.

## Not built

Honest list of what a serious miner could do that this does not.

- **A live channel pool.** The reference implementation tracks up to 199
  channels and switches within seconds of one going offline. This follows only
  the channel it is watching, so it learns about *that* one immediately but
  still discovers a better channel appearing only on its own timer.
- **Request batching and a rate limiter.** Twitch's GraphQL endpoint accepts an
  array of operations; the reference implementation sends 20 at a time under a
  hard 5/sec cap. Every call here is its own request, bounded only by
  concurrency limits.
- **Logging in from the dashboard.** A dead token still needs shell access.
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

354 tests, no clippy warnings.

Well covered: the engine against a scripted Twitch (93 scenarios), strategies,
eligibility and feasibility maths, the watchdog ladder, parsing, the control
plane end to end over HTTP, retry behaviour against a real socket, and the
dashboard rendered in jsdom against a real snapshot.

Thin: `run.rs`'s outer loop sequencing and the PubSub supervisor are still
verified by running the binary rather than by tests — the dynamic
subscription's plumbing is unit-tested, but nothing proves the supervisor
re-subscribes correctly after a reconnect except watching it. The JSON fixtures
are hand-built from observed shapes rather than captured responses.
