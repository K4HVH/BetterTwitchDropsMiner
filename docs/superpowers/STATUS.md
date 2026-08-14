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

**From attacking the running dashboard.** A 200 KB paste into any settings box
was accepted, written to `config.toml` and then carried in every SSE frame for
the life of the process. And one unexpected field in a snapshot blanked the
page permanently: `render()` emptied `#app` before rebuilding into it, and is
called inside the stream handler's try/catch, so the throw was swallowed and
the next heartbeat carried the same field. No hero, no tabs, no way back.

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
- **Per-drop targeting.** "Get me tier 1 and stop" cannot be expressed. Priority
  entries do now match campaign names as well as game names, so two campaigns
  for the same game can be ranked against each other, but the unit of both
  priority and exclusion is still a whole campaign.
- **Subscription rewards, deliberately.** Campaigns whose drops require a
  subscription are identified and reported, never mined — buying subs is not a
  decision a miner gets to make on someone's behalf.
- **Translations.** English only.
- **A keyring.** Tokens are plaintext at mode 0600, documented as a deliberate
  trade-off for headless boxes.

## What Twitch changed under us

Worth recording separately, because these were not mistakes in the code when it
was written — the API moved.

- **`viewerDropCampaigns` stopped listing game campaigns.** The per-channel
  availability check now returns only sitewide reward campaigns: every
  drops-enabled Escape from Tarkov streamer reports exactly one campaign, an
  Audible subscription reward, while visibly earning that game's drops. Used as
  a veto, as it had been, it rejected every candidate on every pass — so the
  strategy was never consulted and selection fell through to a fallback that
  took the alphabetically first channel. It is a rescue now, never a veto,
  which is how the reference implementation has always used it.
- **The campaign detail query returns no box art.** Checked against a live
  account: 124 of 124 campaigns carry `boxArtURL` in the dashboard listing, and
  0 of 6 in the detail response. Presentation fields are merged field by field
  rather than the detail response replacing the listing whole.
- **Integrity challenges.** `ViewerDropsDashboard` and `DropCampaignDetails`
  answer `IntegrityCheckFailed` to a bare client. The miner's own requests are
  not currently challenged; nothing implements the integrity handshake, and if
  Twitch starts challenging in earnest campaign discovery stops working. This
  is the most likely thing to break next.

## Test coverage, and where it is thin

491 Rust tests and 123 dashboard checks, no clippy warnings.

Well covered: the whole binary end to end against a fake Twitch, the engine
against a scripted one (100+ scenarios), hostile data at every layer, strategies,
eligibility and feasibility maths, the watchdog ladder, parsing, the control
plane end to end over HTTP, retry behaviour against a real socket, and the
dashboard rendered in jsdom against a real snapshot.

`run.rs`'s loop is now covered too: six end-to-end tests drive the whole binary
against a fake Twitch — stored token, validation, sync, selection, discovering
the spade endpoint by scraping, the heartbeat, the poll, and claiming a
finished drop. Every URL is overridable by environment variable, which is what
made that possible; the binary could not be tested at all before.

PubSub is covered against a real websocket server too: acknowledgement, a
rejected subscription, a server that accepts and then says nothing, a dropped
connection being reported, changing subscriptions on a live socket in
correctly-sized frames, unsubscribing, and every frame carrying the token.
Each of those failure modes produces a socket that looks perfectly healthy and
delivers nothing.

Two adversarial reviews of this work found defects the tests did not: the
availability veto still living in the recovery ladder, where it quarantined
every channel it looked at; a confirmation *deleting* campaigns from a
candidate rather than adding confidence; an earned subscription reward that
would never be claimed; three commands reporting a reselect that never
happened; and a recovery ladder that could not climb past the rung that
switches channel. Each is now covered.

Thin: the supervisor's own reconnect *loop* — the backoff and the decision to
try again — is still exercised only by running the binary; the protocol
underneath it is not. The JSON fixtures are hand-built from observed shapes
rather than captured responses.

**The gap that mattered most, and what closed it.** Every test passed while the
miner selected channels alphabetically for hours, because the scripted Twitch
always answered the availability check with exactly the drops the test wanted.
The failure needed Twitch to answer with a drop from an *unrelated* campaign,
which no fixture ever did. Nine tests now model what Twitch actually returns,
and each one fails against the previous behaviour — that was checked by
restoring it, not assumed.

The lesson generalises: a fake that only ever returns the useful answer tests
the happy path twice and the real world never. The same blind spot hid the
campaign artwork bug (fixtures always carried box art in both responses) and
the subscription rewards (no fixture had `requiredSubs`).
