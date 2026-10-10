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
  drop reaching 60/60 and the next tier taking over — until 2026-10-08, when
  spade stopped earning anything (see "What Twitch changed under us")
- The playback player, on its own with nothing else watching (2026-10-10):
  CFB Live 7 → 11 of 15 over four minutes, and on the final code 28 → 30 of
  30, the next tier taking over and continuing 31 → 32 of 60, every heartbeat
  reporting video served
- Campaign switching, the feasibility filter abandoning an impossible campaign
  unprompted, the recovery ladder firing at exactly 360s on a non-earning
  channel
- Every claim the dashboard makes checked back against Twitch across 11
  campaigns and 40 tiers — status, windows, allow-lists, requirements,
  progress, claims and the totals — with no disagreements
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

**From auditing the dashboard against Twitch.** Four defects, all of which
looked like working software from every angle except Twitch's:

- The awarded-benefit ledger applied only to campaigns already in the
  inventory, which are the ones that least need it — see *What Twitch changed
  under us*. 24½ hours of watching queued for rewards already owned.
- Feasibility was all-or-nothing: a campaign whose *last* tier had slipped out
  of reach was dropped from `wants` entirely, so no directory query was ever
  made for it. Twitch's daily Tarkov campaign — nine tiers, 540 minutes, a
  24-hour window — was abandoned with 522 minutes left and eight tiers still
  winnable. Feasibility now asks whether *any* tier is reachable, and slack is
  measured against the furthest one that is, so a campaign is never ranked by
  an outcome already lost.
- `live_channels` was a bare count in which zero meant both "nobody is
  streaming this" and "we never looked" — the second being what a paused miner
  reported for everything it had. It is an `Option` now, and the page says
  "not checked" rather than asserting something nobody checked.
- The same count was clamped by `max_channels_per_campaign`, a budget for how
  many channels are worth *verifying*, so a game with thirty drops-enabled
  streamers live reported eight.

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

- **Spade stopped earning drop time (2026-10-08).** A `minute-watched` event
  is still accepted with a 204 and changes nothing. On this account an R6S drop
  crept from 3 to 6 of 60 minutes over eight hours of repeated visits on
  2026-10-09, and the miner spent the day leaving channel after channel as
  "earning nothing" — correctly, as it turned out. Twitch now credits an
  account that is actually fetching the stream: on 2026-10-10, HEAD requests
  for a stream's video segments took EXFIL Brothers Day 2 from no drop session
  at all to 4 minutes in under five, with nothing else changed. The reference
  implementation and its forks hit the same wall the same day.

  The miner now plays the stream (`btdm-twitch/src/playback.rs`): a playback
  token, the stream's playlist, and a HEAD for each new segment every ten
  seconds, on its own clock because a once-a-minute heartbeat would leave half
  of each thirty-second playlist unrequested. The video itself is never
  downloaded. The heartbeat now reports success only if video was actually
  served since the last one. Spade is still sent, as a web player does, but
  nothing depends on it.

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
- **A campaign's own `self.isClaimed` is not a record of what you own.** It
  tracks claims made while the campaign sits in the in-progress inventory, and
  a campaign leaves that list as soon as every tier is claimed. Ask about it
  afterwards through the dashboard or detail query and Twitch reports it
  untouched: zero minutes watched, nothing claimed. Verified against the live
  account on five campaigns at once — R6S Year 11 Season 2 reported its MP5SD
  unclaimed at 0 of 180 minutes six days after awarding it, and the day's
  Escape from Tarkov campaign reported all nine tiers unclaimed the morning
  after winning them hourly from 14:07 to 22:21.

  `gameEventDrops` is the durable record. It is the account's whole
  awarded-benefit history, so it must be matched against each tier's own
  window: Twitch reuses benefit ids across reruns of a campaign, and without
  the window check a skin won last season marks this season's identical tier
  claimed. Daily campaigns like Tarkov's mint fresh benefit ids per day, so the
  check is belt-and-braces there, but the seasonal ones do not.

  Cost of getting this wrong, measured: 5 of 16 supposedly-mineable campaigns
  were already finished, and the miner had 24½ hours of watching queued for
  rewards the account already owned.

## Test coverage, and where it is thin

538 Rust tests and 127 dashboard checks, no clippy warnings, plus a live audit
against Twitch itself (`tools/audit-live/`).

**The check that was missing entirely.** Everything above asks whether the
miner does what it intends. Nothing asked whether what it believes is *true*,
and that is where the worst bugs sat: every layer faithfully relaying a wrong
answer, all tests green. `tools/audit-live/audit.py` puts the dashboard's
claims back to Twitch — status, windows, allow-lists, per-tier requirements and
progress, each tier's claimed state cross-checked three ways, and the
arithmetic each card asserts about itself — and exits non-zero on any
disagreement. It is verified falsifiable: corrupting a snapshot six different
ways is caught six times out of six, and the README says how to re-confirm
that.

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

**And the sharper version of it.** A fake cannot disagree with itself. Both of
the fixes above turned on Twitch giving two different answers to the same
question — `isClaimed` against `gameEventDrops`, the campaign list against the
detail response — and no fixture built from one observed shape can express
that. The feasibility bug was the mirror image: not a wrong answer from
Twitch, but a question the miner declined to ask, and the resulting "nobody
live" was indistinguishable from a true one because a count of zero meant both
things. Tests that only see what the miner sends cannot catch either. That is
what `tools/audit-live/` is for.
