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
node check.js
```

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
