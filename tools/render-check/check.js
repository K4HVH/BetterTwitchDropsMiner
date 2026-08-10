// Renders the real dashboard against a real snapshot in jsdom, then drives
// every tab and control. This is the only way to prove the page does not throw
// on live data — the Rust tests can only prove the payload is well formed.
const fs = require("fs");
const path = require("path");
const { JSDOM } = require("jsdom");

const here = __dirname;
const html = fs.readFileSync(
  path.join(here, "../../crates/btdm-web/assets/index.html"),
  "utf8",
);
const snapPath = process.argv[2] || path.join(here, "snapshot.json");
if (!fs.existsSync(snapPath)) {
  console.error(
    `No snapshot to render.\n\n` +
    `Capture one from a running miner:\n` +
    `  curl -s localhost:8080/api/snapshots > ${snapPath}\n\n` +
    `or pass a path: node tools/render-check/check.js /path/to/snapshot.json`,
  );
  process.exit(2);
}
const snapshots = JSON.parse(fs.readFileSync(snapPath, "utf8"));

const errors = [];
const posted = [];

const dom = new JSDOM(html, {
  runScripts: "dangerously",
  url: "http://localhost:8080/",
  pretendToBeVisual: true,
  beforeParse(window) {
    window.addEventListener("error", (e) => errors.push("window error: " + e.message));

    window.fetch = async (url, opts) => {
      if (opts && opts.method === "POST") {
        posted.push({ url, body: JSON.parse(opts.body) });
        return { ok: true, status: 200, json: async () => ({ ok: true }) };
      }
      return { ok: true, status: 200, json: async () => snapshots };
    };

    // A stub that lets us push frames by hand.
    class FakeEventSource {
      constructor(url) {
        this.url = url;
        this.listeners = {};
        window.__es = this;
      }
      addEventListener(type, fn) { (this.listeners[type] ||= []).push(fn); }
      close() {}
      emit(type, data) {
        (this.listeners[type] || []).forEach((fn) => fn({ data: JSON.stringify(data) }));
      }
    }
    window.EventSource = FakeEventSource;
  },
});

const { window } = dom;
const doc = window.document;

function check(label, fn) {
  try {
    fn();
    console.log("  ok   " + label);
  } catch (e) {
    errors.push(`${label}: ${e.stack.split("\n").slice(0, 3).join(" | ")}`);
    console.log("  FAIL " + label + " -> " + e.message);
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  await sleep(250); // let poll() resolve and render

  console.log("Rendering against a live snapshot:");
  const s = snapshots[0];
  console.log(`  (${s.eligible.length} campaigns, ${s.live.length} live, ${s.rejected.length} rejected)`);

  check("page rendered something", () => {
    const app = doc.querySelector("#app");
    if (app.textContent.includes("Waiting for the miner")) throw new Error("still the placeholder");
    if (app.children.length === 0) throw new Error("app is empty");
  });

  check("the watched channel is shown", () => {
    if (!s.watching) return;
    const t = doc.querySelector("#app").textContent;
    if (!t.includes(s.watching.channel_display_name)) throw new Error("channel name missing");
    if (!t.includes(s.watching.reason.slice(0, 20))) throw new Error("selection reason missing");
  });

  check("drops and benefits are rendered", () => {
    const drops = doc.querySelectorAll(".drop");
    if (drops.length === 0) throw new Error("no drop rows rendered");
    const imgs = doc.querySelectorAll(".drop img");
    console.log(`       ${drops.length} drop rows, ${imgs.length} with artwork`);
  });

  check("box art is rendered", () => {
    const art = doc.querySelectorAll("img.art");
    if (art.length === 0) throw new Error("no campaign artwork");
    for (const a of art) {
      if (!a.src.startsWith("https://static-cdn.jtvnw.net/")) {
        throw new Error("unexpected image host: " + a.src);
      }
    }
  });

  // Every tab must render without throwing.
  for (const label of ["Live now", "Unavailable", "Settings", "Activity", "Campaigns"]) {
    check(`tab: ${label}`, () => {
      const btn = [...doc.querySelectorAll(".tabs button")].find((b) =>
        b.textContent.startsWith(label));
      if (!btn) throw new Error("tab button missing");
      btn.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
      if (doc.querySelector("#app").children.length === 0) throw new Error("empty after switching");
    });
  }

  check("search filters campaigns", () => {
    const search = doc.querySelector("#search");
    if (!search) throw new Error("no search box on the campaigns tab");
    const before = doc.querySelectorAll(".camp").length;
    search.value = "zzzz-no-such-campaign";
    search.dispatchEvent(new window.Event("input"));
    const after = doc.querySelectorAll(".camp").length;
    if (after !== 0) throw new Error(`filter did nothing: ${before} -> ${after}`);
    search.value = "";
    doc.querySelector("#search").dispatchEvent(new window.Event("input"));
  });

  check("controls post commands", async () => {
    posted.length = 0;
    const buttons = [...doc.querySelectorAll("button")];
    const pause = buttons.find((b) => /Pause mining|Resume mining/.test(b.textContent));
    if (!pause) throw new Error("no pause button");
    pause.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  });
  await sleep(60);
  check("the pause command reached the server", () => {
    if (!posted.length) throw new Error("nothing was posted");
    if (!/\/api\/accounts\/.+\/command/.test(posted[0].url)) throw new Error("wrong URL: " + posted[0].url);
    if (!["pause", "resume"].includes(posted[0].body.command)) {
      throw new Error("wrong payload: " + JSON.stringify(posted[0].body));
    }
    console.log(`       POST ${posted[0].url} ${JSON.stringify(posted[0].body)}`);
  });

  check("strategy picker posts the choice", () => {
    posted.length = 0;
    const sel = doc.querySelector("#app select");
    if (!sel) throw new Error("no strategy picker");
    sel.value = "completionist";
    sel.dispatchEvent(new window.Event("change"));
  });
  await sleep(60);
  check("the strategy command was sent", () => {
    if (!posted.length) throw new Error("nothing posted");
    if (posted[0].body.command !== "set_strategy" || posted[0].body.name !== "completionist") {
      throw new Error("wrong payload: " + JSON.stringify(posted[0].body));
    }
  });

  // Streamed frames.
  check("a pushed snapshot re-renders", () => {
    const modified = JSON.parse(JSON.stringify(s));
    modified.state = "Idle";
    modified.watching = null;
    window.__es.emit("snapshot", modified);
    if (!doc.querySelector("#app").textContent.includes("Idle")) {
      throw new Error("the pushed snapshot did not render");
    }
  });

  check("a log frame is displayed and escaped", () => {
    window.__es.emit("log", {
      account: "default",
      at: new Date().toISOString(),
      severity: "good",
      summary: "<img src=x onerror=alert(1)>Claimed Something",
      kind: {},
    });
    const btn = [...doc.querySelectorAll(".tabs button")].find((b) => b.textContent.startsWith("Activity"));
    btn.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    const log = doc.querySelector("#log");
    if (!log.textContent.includes("Claimed Something")) throw new Error("log line missing");
    if (log.querySelector("img")) throw new Error("XSS: markup from the payload was parsed as HTML");
  });

  // Hostile / degenerate payloads.
  check("an empty snapshot does not crash the page", () => {
    window.__es.emit("snapshot", {
      account: s.account, login: null, strategy: "deadline", state: "Idle",
      paused: false, generated_at: new Date().toISOString(), watching: null,
      eligible: [], rejected: [], live: [],
      settings: { strategy: "deadline", priority_games: [], favourites: [], pinned: null,
                  excluded_campaigns: [], excluded_games: [] },
      strategies: [], health: { verdict: "idle", seconds_since_confirmation: null,
                                recovery_step: null, last_sync: null, next_sync_in_seconds: null,
                                quarantined_channels: [], quarantined_campaigns: [] },
      totals: { campaigns_known: 0, campaigns_eligible: 0, campaigns_at_risk: 0,
                drops_claimed: 0, drops_remaining: 0, drops_claimable: 0, minutes_remaining: 0 },
    });
  });

  check("a campaign with no drops or artwork does not crash", () => {
    const weird = JSON.parse(JSON.stringify(s));
    weird.account = s.account;
    weird.eligible = [{
      id: "x", name: null, game: "G", game_id: "g", box_art_url: null,
      starts_at: s.generated_at, ends_at: s.generated_at,
      remaining_minutes: 0, slack_minutes: -5, at_risk: true, excluded: false,
      is_current: false, live_channels: 0, allowed_channels: [], drops: [],
    }];
    window.__es.emit("snapshot", weird);
    const btn = [...doc.querySelectorAll(".tabs button")].find((b) => b.textContent.startsWith("Campaigns"));
    btn.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  });

  check("no invented progress is displayed", () => {
    // With no confirmation, the page must say so rather than showing a number.
    const noconfirm = JSON.parse(JSON.stringify(s));
    noconfirm.account = s.account;
    if (noconfirm.watching) {
      noconfirm.watching.current_minutes = null;
      noconfirm.watching.required_minutes = null;
      noconfirm.watching.drop_name = null;
    }
    window.__es.emit("snapshot", noconfirm);
    const t = doc.querySelector("#app").textContent;
    if (!t.includes("No progress confirmed by Twitch yet")) {
      throw new Error("the page must admit it has no confirmation");
    }
  });

  // --- regression: a snapshot arriving mid-typing must not wipe the box ---
  check("typing survives a pushed snapshot", () => {
    const settings = [...doc.querySelectorAll(".tabs button")].find((b) =>
      b.textContent.startsWith("Settings"));
    settings.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));

    const box = doc.querySelector("#priority-input");
    if (!box) throw new Error("no priority-game input");
    box.focus();
    box.value = "Deadl";
    box.dispatchEvent(new window.Event("input"));

    // The miner confirms a minute of progress, as it does every heartbeat.
    const pushed = JSON.parse(JSON.stringify(s));
    pushed.totals.drops_claimed += 1;
    window.__es.emit("snapshot", pushed);

    const after = doc.querySelector("#priority-input");
    if (!after) throw new Error("the input vanished");
    if (after.value !== "Deadl") {
      throw new Error(`half-typed text was wiped: got ${JSON.stringify(after.value)}`);
    }
    if (doc.activeElement !== after) throw new Error("focus was lost mid-word");
  });

  check("the pin box keeps an edit instead of snapping back", () => {
    const pin = doc.querySelector("#pin-input");
    if (!pin) throw new Error("no pin input");
    pin.value = "somestreamer";
    pin.dispatchEvent(new window.Event("input"));
    window.__es.emit("snapshot", JSON.parse(JSON.stringify(s)));
    if (doc.querySelector("#pin-input").value !== "somestreamer") {
      throw new Error("the pin edit was discarded");
    }
  });

  check("a snapshot for another account does not rebuild the page", () => {
    const other = JSON.parse(JSON.stringify(s));
    other.account = "second-account";
    const box = doc.querySelector("#pin-input");
    box.focus();
    window.__es.emit("snapshot", other);
    // Rendering for a background account would blow away focus here.
    if (doc.activeElement !== doc.querySelector("#pin-input")) {
      throw new Error("a background account's update stole focus");
    }
  });

  console.log("");
  if (errors.length) {
    console.log(`FAILURES (${errors.length}):`);
    errors.forEach((e) => console.log("  - " + e));
    process.exit(1);
  }
  console.log("All render checks passed.");
  process.exit(0);
})();
