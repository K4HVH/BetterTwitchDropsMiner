# Dashboard render check

The dashboard is a single hand-written HTML file with no build step, which is
deliberate — `cargo build` alone produces the finished binary. The cost is that
nothing in `cargo test` can prove the page actually *renders*: the Rust tests
can only prove the JSON payload is well formed.

This closes that gap. It loads the real `index.html` into jsdom, feeds it a real
snapshot captured from a running miner, then drives every tab and control and
asserts on the resulting DOM. It caught a page that rendered nothing when a
campaign had no drops, and it is the only check that the activity log escapes
what the server sends it.

It is not part of `cargo test` on purpose: requiring Node to build a Rust binary
would undo the reason the page has no build step. Run it by hand when changing
the page.

## Running it

```sh
# with a miner running
curl -s localhost:8080/api/snapshots > tools/render-check/snapshot.json
cd tools/render-check && npm install jsdom     # once
node check.js          # does it work on real data?
node adversarial.js    # can it be broken?          (needs no snapshot)
node live.js           # drive the real thing       (needs a running miner)
```

`live.js` is the only check that covers the whole path — browser to axum to
the engine and back. It loads the page from the miner's own server, opens a
real SSE stream, clicks real controls, and then asks the miner over a separate
connection whether it actually did what it was told. It changes strategy,
pauses, excludes a campaign and stars a channel, and puts all of it back
afterwards. Run it against a miner you do not mind poking.

Exit status is 0 when every check passes, 1 on a failure, 2 when there is no
snapshot to render.

## What it asserts

- The page renders campaigns, drops, benefit artwork and box art from real data
- Every tab renders without throwing, including on an empty snapshot and on a
  campaign with no drops and no artwork
- The search box filters
- Controls POST the right command to the right URL
- A pushed snapshot re-renders the page, and a pushed log line appears
- Markup in a server-supplied string is displayed as text, never parsed as HTML
- With no confirmation from Twitch, the page says so rather than showing a number

## What `adversarial.js` asserts

It treats the snapshot as untrusted, which it very nearly is — campaign names,
drop names, stream titles and channel display names are all chosen by whoever
made the channel or campaign.

- Script injection in every string field: campaigns, drops, benefits, the
  watched channel, stream titles, selection reasons, rejection reasons,
  settings chips, and the activity log
- `javascript:` URLs in link targets and image sources, which it found being
  rendered into an anchor the user is actively encouraged to click
- Null, missing and wrong-typed fields everywhere, including whole absent
  sections — which it found leaving the page half-rendered with no tabs
- Negative, zero, enormous, null and non-numeric minutes; dates in 1970 and
  9999 and dates that are not dates
- 1000 campaigns with 8000 drops, filtering over them, 5000-character names,
  emoji and right-to-left text
- Twenty clicks on one control, a command the server refuses, and the token
  surviving into every request
- URL metacharacters in campaign ids and account names
- Twenty snapshots landing while the user is mid-word, with focus and caret
- That nothing invented ever reaches the screen: no confirmation shows no
  number, a stall is unmistakable, and a claimed drop has no progress bar
