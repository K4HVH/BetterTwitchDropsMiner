# Status — 2026-08-10

Built in one session, verified against a real Twitch account (`k4hvh`).

## Working, confirmed against the live API

- **Device-code login** — code shown, entered at twitch.tv/activate, token stored.
- **Campaign discovery** — 61 campaigns merged from inventory + dashboard +
  per-campaign details; 7–8 eligible after server-verified link checks.
- **Watch heartbeat** — spade POST accepted (204).
- **Server-confirmed progress** — observed climbing 161 → 173 / 180 minutes in
  real time, via both the PubSub push and the poll backstop.
- **Campaign switching** — moved to a new campaign and began earning on it.
- **Feasibility filter** — abandoned a campaign that could no longer finish
  before its deadline and chose a different one, unprompted.
- **Dashboard** — serves on 127.0.0.1:8080, renders live snapshots and streams
  events over SSE.
- **Watchdog** — on a channel that never confirmed anything, the ladder fired at
  exactly the 360s threshold with `step=ResendWatch`. This is the case the tool
  this replaces cannot even detect: it would have shown a moving progress bar
  indefinitely.

## v1 scope

| Item | State |
|---|---|
| Timed drops | Done |
| Badge/emote campaigns with verified eligibility | Done |
| Channel points | Done (claims on the watched channel only) |
| Five strategies + favourites tie-break | Done |
| Feasibility and deadline maths | Done |
| Watchdog + seven-rung recovery ladder | Done |
| Multi-account, independent | Done |
| Web dashboard, embedded, token-guarded | Done |
| Discord alerting | Done |
| Docker, systemd, static release workflow | Done |
| Self-update | Done (explicit, checksum-verified, never automatic) |

Self-update installs only when asked (`btdm update --apply`), verifies the
published SHA-256 before replacing anything, refuses a release with no
checksum, and swaps by rename. Nothing updates itself behind your back, because
an unattended miner that replaces its own binary mid-run is the easiest way to
turn a working box into a broken one.

## Defects found and fixed during the build

Found by probing the real API:

1. **Game shape mismatch.** The campaign dashboard names games with
   `displayName` and omits `slug`; the inventory uses `name` and `slug`.
   Requiring `name` discarded 129 of 130 campaigns — the miner saw one.
2. **Blocked spade host.** `spade.twitch.tv` was null-routed by an ad-blocker.
   It is the only endpoint that advances drop time, so every miner silently
   earns nothing when it is blocked. Now diagnosed by name.
3. **Misleading status line.** Displayed `159/240` for one drop while the drop
   actually confirming was at `161/180`.

Found by adversarial review:

4. **`logout` reported success** when no token existed.
5. **Terminal OAuth errors** were indistinguishable from `authorization_pending`,
   so a broken flow polled silently for 30 minutes then blamed the user.
6. **Progress was account-wide, not session-scoped.** Watching Twitch in a
   browser would reset the miner's stall clock with the browser's progress —
   the watchdog would never fire while the miner earned nothing. Compounded by
   an earlier fix of mine that stripped the scoping variables from the query.
7. **PubSub reconnection ran inline**, blocking heartbeats and Ctrl-C, and a
   single failed reconnect disabled push events permanently.
8. **Spade URL cache** was invalidated on rate limits, causing rediscovery
   storms while already throttled.
9. **`Unauthorized` mid-session** churned for up to 30 minutes before reporting.
10. **PubSub `LISTEN` was never acknowledged**, so a rejected subscription
    looked identical to a healthy socket.

## Late fixes

- **`btdm status` reported "token valid for ~0 days"** for a token that never
  expires — Twitch signals non-expiry as `expires_in: 0`, and dividing by a day
  turned that into a message reading as imminent expiry.
- **The dashboard announced the requested address, not the bound one**, so
  binding to port 0 printed a dead URL. Found immediately by the first
  HTTP-level test written for the web crate.

## Incident during the build

`devdata/state.db`, containing a live OAuth token, was committed by a broad
`git add -A`. Nothing had been pushed — `origin/main` was still at the
pre-session commit — so the token never left the machine. History was rewritten,
backup refs and reflog expired, and the token string verified absent from every
remaining object. `devdata/`, `*.db` and `config.toml` are now in `.gitignore`.

## Known gaps

- `run.rs` remains the least-tested file. Its recovery ladder is exercised
  through `btdm-core` tests, but the loop itself is verified by running it.
- Fixture JSON is hand-built from observed shapes. Captured real responses would
  be stronger.
- Token refresh is not implemented. This turned out not to matter: Twitch
  reports `expires_in: 0` for these device-flow tokens, meaning they do not
  expire. A refresh token is stored should that ever change.
