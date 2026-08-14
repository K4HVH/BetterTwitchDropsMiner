// Hostile testing of the dashboard.
//
// `check.js` proves the page works on good data. This one tries to break it:
// script injection in every string the server can send, missing and null
// fields, absurd numbers, enormous payloads, hammering the controls, and
// updates landing mid-interaction.
//
// Everything here treats the snapshot as untrusted. It very nearly is —
// campaign names, drop names, stream titles and channel display names are all
// attacker-controlled in the sense that anyone can create a Twitch channel and
// name it whatever they like.
const fs = require("fs");
const path = require("path");
const { JSDOM } = require("jsdom");

const here = __dirname;
const html = fs.readFileSync(
  path.join(here, "../../crates/btdm-web/assets/index.html"),
  "utf8",
);

const failures = [];
let checks = 0;

function check(label, fn) {
  checks++;
  try {
    fn();
    console.log("  ok   " + label);
  } catch (e) {
    failures.push(`${label}: ${e.message}`);
    console.log("  FAIL " + label + " -> " + e.message);
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// --------------------------------------------------------------------------
// Payload builders
// --------------------------------------------------------------------------

const XSS = `<img src=x onerror="window.__pwned=1"><script>window.__pwned=1</script>`;

function drop(over = {}) {
  return {
    id: "d1",
    name: "Drop",
    current_minutes: 5,
    required_minutes: 60,
    percent: 8.3,
    is_claimed: false,
    claimable: false,
    locked: false,
    active: false,
    benefits: [{ name: "Thing", kind: "Entitlement", image_url: null }],
    ...over,
  };
}

function campaign(over = {}) {
  return {
    id: "c1",
    name: "Campaign",
    game: "Game",
    game_id: "g1",
    box_art_url: null,
    account_link_url: null,
    starts_at: new Date(Date.now() - 86400000).toISOString(),
    ends_at: new Date(Date.now() + 86400000).toISOString(),
    remaining_minutes: 55,
    slack_minutes: 1385,
    at_risk: false,
    excluded: false,
    is_current: false,
    live_channels: 3,
    allowed_channels: [],
    drops: [drop()],
    ...over,
  };
}

function snapshot(over = {}) {
  return {
    account: "default",
    login: "someone",
    strategy: "deadline",
    state: "Mining",
    paused: false,
    generated_at: new Date().toISOString(),
    watching: {
      channel_login: "streamer",
      channel_display_name: "Streamer",
      stream_title: "playing a game",
      viewers: 1234,
      campaign: "Campaign",
      campaigns: ["Campaign"],
      reason: "deadline: advances Campaign",
      since: new Date().toISOString(),
      drop_name: "Drop",
      current_minutes: 5,
      required_minutes: 60,
      seconds_since_confirmation: 12,
      stalled: false,
    },
    eligible: [campaign()],
    rejected: [],
    live: [
      {
        login: "streamer",
        display_name: "Streamer",
        viewers: 1234,
        title: "playing a game",
        game: "Game",
        campaigns: ["Campaign"],
        drops: 1,
        is_current: true,
        is_favourite: false,
      },
    ],
    settings: {
      strategy: "deadline",
      priority_games: [],
      favourites: [],
      pinned: null,
      excluded_campaigns: [],
      excluded_games: [],
    },
    strategies: [
      { name: "deadline", description: "Soonest deadline.", selected: true },
      { name: "coverage", description: "Most campaigns.", selected: false },
    ],
    health: {
      verdict: "healthy",
      seconds_since_confirmation: 12,
      recovery_step: null,
      last_sync: new Date().toISOString(),
      next_sync_in_seconds: 300,
      quarantined_channels: [],
      quarantined_campaigns: [],
    },
    totals: {
      campaigns_known: 1,
      campaigns_eligible: 1,
      campaigns_at_risk: 0,
      drops_claimed: 0,
      drops_remaining: 1,
      drops_claimable: 0,
      minutes_remaining: 55,
    },
    ...over,
  };
}

// --------------------------------------------------------------------------
// Harness
// --------------------------------------------------------------------------

const posted = [];
let failNextPost = false;
let snapshots = [snapshot()];

const dom = new JSDOM(html, {
  runScripts: "dangerously",
  url: "http://localhost:8080/?token=s3cret",
  pretendToBeVisual: true,
  beforeParse(window) {
    window.addEventListener("error", (e) =>
      failures.push("uncaught page error: " + e.message));
    window.__pwned = 0;

    window.fetch = async (url, opts) => {
      if (opts && opts.method === "POST") {
        posted.push({ url, body: JSON.parse(opts.body || "{}") });
        if (failNextPost) {
          failNextPost = false;
          return {
            ok: false,
            status: 503,
            json: async () => ({ ok: false, error: "the miner is busy" }),
          };
        }
        return { ok: true, status: 200, json: async () => ({ ok: true }) };
      }
      if (String(url).includes("/api/history")) {
        return { ok: true, status: 200, json: async () => [] };
      }
      if (String(url).includes("/api/rewards")) {
        return { ok: true, status: 200, json: async () => [] };
      }
      if (String(url).includes("/api/logins")) {
        return { ok: true, status: 200, json: async () => ({ logins: [], errors: {} }) };
      }
      return { ok: true, status: 200, json: async () => snapshots };
    };

    class FakeEventSource {
      constructor(url) {
        this.url = url;
        this.listeners = {};
        window.__es = this;
      }
      addEventListener(t, fn) { (this.listeners[t] ||= []).push(fn); }
      close() {}
      emit(t, data) {
        (this.listeners[t] || []).forEach((fn) => fn({ data: JSON.stringify(data) }));
      }
      raw(t, text) {
        (this.listeners[t] || []).forEach((fn) => fn({ data: text }));
      }
    }
    window.EventSource = FakeEventSource;
  },
});

const { window } = dom;
const doc = window.document;
const push = (s) => window.__es.emit("snapshot", s);
/// Forces the page to display a given account, as choosing it in the picker
/// would. Snapshots for other accounts deliberately do not steal the view.
const state_reset = (account) => {
  const picker = doc.querySelector("#account-picker select");
  window.__forceAccount = account;
  if (picker) {
    picker.value = account;
    picker.dispatchEvent(new window.Event("change"));
  }
};
const text = () => doc.querySelector("#app").textContent;
const click = (node) =>
  node.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
const clearFilter = () => {
  const box = doc.querySelector("#search");
  if (!box) return;
  box.value = "";
  box.dispatchEvent(new window.Event("input"));
};
const tab = (label) => {
  const b = [...doc.querySelectorAll(".tabs button")].find((x) =>
    x.textContent.startsWith(label));
  if (!b) throw new Error(`no tab "${label}"`);
  click(b);
};

(async () => {
  await sleep(250);
  console.log("Adversarial dashboard testing\n");

  // ---------------------------------------------------------------- injection
  console.log("Script injection:");

  check("markup in campaign and drop names is not executed", () => {
    push(snapshot({
      eligible: [campaign({
        name: XSS,
        game: XSS,
        drops: [drop({ name: XSS, benefits: [{ name: XSS, kind: XSS, image_url: null }] })],
      })],
    }));
    tab("Campaigns");
    if (doc.querySelector("#app img[src='x']")) throw new Error("markup was parsed as HTML");
    if (window.__pwned) throw new Error("script executed");
    if (!text().includes("<img")) throw new Error("the text was dropped instead of escaped");
  });

  check("markup in the watched channel and reason is not executed", () => {
    push(snapshot({
      watching: {
        ...snapshot().watching,
        channel_display_name: XSS,
        stream_title: XSS,
        reason: XSS,
        campaigns: [XSS],
        drop_name: XSS,
      },
    }));
    if (doc.querySelector("#app img[src='x']")) throw new Error("markup was parsed as HTML");
    if (window.__pwned) throw new Error("script executed");
  });

  check("markup in the live-channel table is not executed", () => {
    push(snapshot({
      live: [{ ...snapshot().live[0], display_name: XSS, title: XSS, campaigns: [XSS] }],
    }));
    tab("Live now");
    if (doc.querySelector("#app img[src='x']")) throw new Error("markup was parsed as HTML");
    if (window.__pwned) throw new Error("script executed");
  });

  check("markup in rejection reasons is not executed", () => {
    push(snapshot({
      rejected: [{
        id: "r1", name: XSS, game: XSS, reason: XSS,
        explanation: XSS, action_url: null,
        starts_at: new Date().toISOString(), ends_at: new Date().toISOString(),
        box_art_url: null,
      }],
    }));
    tab("Unavailable");
    if (doc.querySelector("#app img[src='x']")) throw new Error("markup was parsed as HTML");
    if (window.__pwned) throw new Error("script executed");
  });

  check("markup in settings chips is not executed", () => {
    push(snapshot({
      settings: {
        ...snapshot().settings,
        priority_games: [XSS],
        favourites: [XSS],
        excluded_games: [XSS],
        pinned: XSS,
      },
    }));
    tab("Settings");
    if (doc.querySelector("#app img[src='x']")) throw new Error("markup was parsed as HTML");
    if (window.__pwned) throw new Error("script executed");
  });

  check("a javascript: URL is never used as a link target", () => {
    push(snapshot({
      rejected: [{
        id: "r1", name: "Evil", game: "G", reason: "not linked",
        explanation: "link it",
        action_url: "javascript:window.__pwned=1",
        starts_at: new Date().toISOString(), ends_at: new Date().toISOString(),
        box_art_url: null,
      }],
    }));
    tab("Unavailable");
    const bad = [...doc.querySelectorAll("#app a")].filter((a) =>
      (a.getAttribute("href") || "").toLowerCase().startsWith("javascript:"));
    if (bad.length) throw new Error(`${bad.length} javascript: link(s) rendered`);
  });

  check("an image URL cannot point at a non-image scheme", () => {
    push(snapshot({
      eligible: [campaign({
        box_art_url: "javascript:window.__pwned=1",
        drops: [drop({
          benefits: [{ name: "x", kind: "y", image_url: "javascript:window.__pwned=1" }],
        })],
      })],
    }));
    tab("Campaigns");
    const bad = [...doc.querySelectorAll("#app img")].filter((i) =>
      (i.getAttribute("src") || "").toLowerCase().startsWith("javascript:"));
    if (bad.length) throw new Error(`${bad.length} javascript: image src rendered`);
  });

  check("a channel login cannot smuggle a scheme into its twitch link", () => {
    push(snapshot({
      live: [{ ...snapshot().live[0], login: "javascript:window.__pwned=1" }],
    }));
    tab("Live now");
    const bad = [...doc.querySelectorAll("#app a")].filter((a) =>
      (a.getAttribute("href") || "").toLowerCase().startsWith("javascript:"));
    if (bad.length) throw new Error("a login became a javascript: link");
  });

  // --------------------------------------------------------------- bad shapes
  console.log("\nMissing and malformed fields:");

  check("null strings everywhere do not crash the page", () => {
    push(snapshot({
      login: null,
      watching: {
        channel_login: null, channel_display_name: null, stream_title: null,
        viewers: null, campaign: null, campaigns: [], reason: null, since: null,
        drop_name: null, current_minutes: null, required_minutes: null,
        seconds_since_confirmation: null, stalled: false,
      },
      eligible: [campaign({ name: null, game: null, drops: [drop({ name: null, benefits: [] })] })],
      live: [{
        login: null, display_name: null, viewers: 0, title: null, game: null,
        campaigns: [], drops: 0, is_current: false, is_favourite: false,
      }],
    }));
    tab("Campaigns");
    tab("Live now");
    if (!doc.querySelector("#app").children.length) throw new Error("page went blank");
  });

  check("absent optional sections do not crash the page", () => {
    const s = snapshot();
    delete s.settings;
    delete s.strategies;
    delete s.health;
    delete s.totals;
    delete s.live;
    delete s.rejected;
    push(s);
    if (!doc.querySelector("#app").children.length) throw new Error("page went blank");
  });

  check("a snapshot that is not an object is ignored, not fatal", () => {
    window.__es.raw("snapshot", "null");
    window.__es.raw("snapshot", "[]");
    window.__es.raw("snapshot", "\"a string\"");
    window.__es.raw("snapshot", "{ this is not json");
    if (!doc.querySelector("#app").children.length) throw new Error("page went blank");
  });

  check("a malformed log frame is ignored", () => {
    window.__es.raw("log", "{ nope");
    window.__es.emit("log", null);
    window.__es.emit("log", { at: "not a date", summary: null, severity: "nonsense" });
    tab("Activity");
    if (!doc.querySelector("#app").children.length) throw new Error("page went blank");
  });

  // ------------------------------------------------------------ absurd values
  console.log("\nAbsurd numbers and dates:");

  check("negative, zero and enormous minutes render sanely", () => {
    push(snapshot({
      eligible: [
        campaign({ id: "a", remaining_minutes: 0, slack_minutes: -99999, at_risk: true,
                   drops: [drop({ current_minutes: 0, required_minutes: 0, percent: 0 })] }),
        campaign({ id: "b", remaining_minutes: 999999999, slack_minutes: 999999999,
                   drops: [drop({ current_minutes: 999999999, required_minutes: 1, percent: 1e9 })] }),
      ],
    }));
    tab("Campaigns");
    const widths = [...doc.querySelectorAll("#app .bar > i")].map((i) => i.style.width);
    for (const w of widths) {
      const pct = parseFloat(w);
      if (!(pct >= 0 && pct <= 100)) throw new Error(`bar width out of range: ${w}`);
    }
  });

  check("NaN and Infinity do not leak into the page", () => {
    push(snapshot({
      totals: { ...snapshot().totals, minutes_remaining: null, drops_claimed: null },
      eligible: [campaign({ drops: [drop({ percent: null, current_minutes: null })] })],
    }));
    tab("Campaigns");
    if (/NaN|Infinity|undefined/.test(text())) {
      throw new Error("a broken number reached the screen");
    }
  });

  check("dates far in the past and future do not crash", () => {
    push(snapshot({
      eligible: [
        campaign({ id: "past", ends_at: "1970-01-01T00:00:00Z" }),
        campaign({ id: "future", ends_at: "9999-12-31T23:59:59Z" }),
        campaign({ id: "broken", ends_at: "not a date" }),
      ],
    }));
    tab("Campaigns");
    if (!doc.querySelector("#app").children.length) throw new Error("page went blank");
  });

  // ------------------------------------------------------------------- volume
  console.log("\nVolume:");

  check("a thousand campaigns render without dying", () => {
    const many = Array.from({ length: 1000 }, (_, i) =>
      campaign({
        id: `c${i}`,
        name: `Campaign ${i}`,
        game: `Game ${i % 40}`,
        drops: Array.from({ length: 8 }, (_, j) => drop({ id: `d${i}-${j}` })),
      }));
    const started = Date.now();
    push(snapshot({ eligible: many }));
    tab("Campaigns");
    const took = Date.now() - started;
    const cards = doc.querySelectorAll("#app .camp").length;
    if (cards !== 1000) throw new Error(`rendered ${cards} of 1000 cards`);
    if (took > 15000) throw new Error(`took ${took}ms`);
    console.log(`       1000 campaigns / 8000 drops in ${took}ms`);
  });

  check("filtering a thousand campaigns works and is not quadratic", () => {
    const search = doc.querySelector("#search");
    if (!search) throw new Error("no search box");
    search.value = "Campaign 42";
    search.dispatchEvent(new window.Event("input"));
    const cards = doc.querySelectorAll("#app .camp").length;
    // "Campaign 42" is a substring of "Campaign 420".."Campaign 429" too, so
    // eleven is the correct answer for a substring filter.
    if (cards !== 11) throw new Error(`filter matched ${cards} cards, expected 11`);
    clearFilter();
    if (doc.querySelectorAll("#app .camp").length !== 1000) {
      throw new Error("clearing the filter did not restore everything");
    }
  });

  check("a very long name cannot escape its container", () => {
    push(snapshot({
      eligible: [campaign({ name: "A".repeat(5000), game: "B".repeat(5000) })],
      watching: { ...snapshot().watching, stream_title: "C".repeat(5000) },
    }));
    tab("Campaigns");
    if (!doc.querySelector("#app").children.length) throw new Error("page went blank");
  });

  check("unicode, emoji and right-to-left text render", () => {
    push(snapshot({
      eligible: [campaign({ name: "🎮 دروبات تويتش 日本語 ‮evil", game: "🕹️" })],
    }));
    tab("Campaigns");
    if (!text().includes("日本語")) throw new Error("unicode was dropped");
  });

  // ----------------------------------------------------------------- controls
  console.log("\nControls under stress:");

  push(snapshot());

  check("the pause button posts exactly one command per click", () => {
    posted.length = 0;
    const pause = [...doc.querySelectorAll("button")].find((b) =>
      /Pause mining|Resume mining/.test(b.textContent));
    click(pause);
    if (posted.length !== 1) throw new Error(`posted ${posted.length} commands`);
  });

  check("hammering a control does not queue a command per click silently", async () => {
    posted.length = 0;
    const pause = [...doc.querySelectorAll("button")].find((b) =>
      /Pause mining|Resume mining/.test(b.textContent));
    for (let i = 0; i < 20; i++) click(pause);
    if (posted.length !== 20) {
      throw new Error(`20 clicks produced ${posted.length} posts`);
    }
    // Every one must be well-formed; a mangled body is worse than a dropped one.
    for (const p of posted) {
      if (!p.body.command) throw new Error("a click produced a body with no command");
    }
  });

  check("a failed command reports it and keeps the page alive", async () => {
    posted.length = 0;
    failNextPost = true;
    const sync = [...doc.querySelectorAll("button")].find((b) =>
      b.textContent.includes("Refresh campaigns"));
    click(sync);
    await sleep(60);
    const toast = doc.querySelector("#toast");
    if (!toast.className.includes("show")) throw new Error("no failure was reported");
    if (!toast.textContent.includes("busy")) {
      throw new Error(`the server's reason was lost: ${toast.textContent}`);
    }
  });

  check("the token from the URL is attached to every request", () => {
    posted.length = 0;
    click([...doc.querySelectorAll("button")].find((b) =>
      b.textContent.includes("Refresh campaigns")));
    if (!posted[0].url.includes("token=s3cret")) {
      throw new Error(`token missing from ${posted[0].url}`);
    }
  });

  check("a campaign id with URL metacharacters is encoded, not injected", () => {
    posted.length = 0;
    push(snapshot({ eligible: [campaign({ id: "a/b?c=d&e#f" })] }));
    tab("Campaigns");
    const exclude = [...doc.querySelectorAll("#app .camp button")].find((b) =>
      b.textContent === "Exclude");
    click(exclude);
    if (posted.length !== 1) throw new Error("the exclude button did nothing");
    if (posted[0].body.campaign !== "a/b?c=d&e#f") {
      throw new Error(`the id was mangled: ${posted[0].body.campaign}`);
    }
  });

  check("an account name with a slash cannot escape its URL path", () => {
    posted.length = 0;
    // The picker only exists once a second account has arrived, so deliver it
    // first and then select it, exactly as a user would.
    push(snapshot({ account: "../../etc/passwd" }));
    state_reset("../../etc/passwd");
    click([...doc.querySelectorAll("button")].find((b) =>
      b.textContent.includes("Refresh campaigns")));
    const url = posted[0].url;
    if (url.includes("../")) throw new Error(`path traversal reached the URL: ${url}`);
    if (!url.includes("%2F")) throw new Error(`the slash was not encoded: ${url}`);
    state_reset("default");
    push(snapshot());
  });

  // ---------------------------------------------------------- race conditions
  console.log("\nUpdates landing mid-interaction:");

  push(snapshot());

  // The priority list is rendered beside the strategy that reads it, so these
  // push snapshots with a priority strategy selected.
  const prioritised = (over = {}) => snapshot({
    strategy: "priority",
    strategies: [
      { name: "deadline", description: "Soonest deadline.", selected: false },
      { name: "priority", description: "Your order first.", selected: true },
    ],
    ...over,
  });

  check("typing survives a burst of twenty snapshots", () => {
    push(prioritised());
    const box = doc.querySelector("#priority-input");
    if (!box) throw new Error("no priority input");
    box.focus();
    box.value = "Half typed";
    box.dispatchEvent(new window.Event("input"));

    for (let i = 0; i < 20; i++) {
      push(prioritised({ totals: { ...snapshot().totals, drops_claimed: i } }));
    }

    const after = doc.querySelector("#priority-input");
    if (after.value !== "Half typed") throw new Error(`lost the text: "${after.value}"`);
    if (doc.activeElement !== after) throw new Error("lost focus");
  });

  check("the caret does not jump to the end mid-word", () => {
    push(prioritised());
    const box = doc.querySelector("#priority-input");
    box.focus();
    box.value = "Deadlock";
    box.dispatchEvent(new window.Event("input"));
    box.setSelectionRange(4, 4);
    push(prioritised());
    const after = doc.querySelector("#priority-input");
    if (after.selectionStart !== 4) {
      throw new Error(`caret moved to ${after.selectionStart}`);
    }
  });

  check("switching tab during a burst keeps the chosen tab", () => {
    tab("Live now");
    for (let i = 0; i < 10; i++) push(snapshot());
    const selected = [...doc.querySelectorAll(".tabs button")]
      .find((b) => b.getAttribute("aria-selected") === "true");
    if (!selected.textContent.startsWith("Live now")) {
      throw new Error(`ended up on ${selected.textContent}`);
    }
  });

  check("the search filter survives a snapshot", () => {
    tab("Campaigns");
    push(snapshot({
      eligible: [campaign({ id: "keep", name: "Keep" }), campaign({ id: "drop", name: "Other" })],
    }));
    const search = doc.querySelector("#search");
    search.value = "Keep";
    search.dispatchEvent(new window.Event("input"));
    push(snapshot({
      eligible: [campaign({ id: "keep", name: "Keep" }), campaign({ id: "drop", name: "Other" })],
    }));
    if (doc.querySelectorAll("#app .camp").length !== 1) {
      throw new Error("the filter was reset by an update");
    }
    clearFilter();
  });

  // ------------------------------------------------------------ the core lie
  console.log("\nThe promise that nothing is invented:");

  check("no confirmation shows no number, not a zero", () => {
    push(snapshot({
      watching: {
        ...snapshot().watching,
        current_minutes: null, required_minutes: null, drop_name: null,
      },
    }));
    const t = text();
    if (!t.includes("No progress confirmed by Twitch yet")) {
      throw new Error("the page did not admit it has no confirmation");
    }
    if (/\b0\/0 min confirmed/.test(t)) throw new Error("it invented a zero");
  });

  check("a stalled session is unmistakable", () => {
    push(snapshot({
      watching: {
        ...snapshot().watching, stalled: true, seconds_since_confirmation: 4000,
      },
    }));
    const t = text();
    if (!t.includes("STALLED")) throw new Error("a stall was not surfaced");
    if (!/recovery ladder/i.test(t)) throw new Error("no explanation of what happens next");
  });

  check("claimed drops never show a progress bar that could imply movement", () => {
    push(snapshot({
      eligible: [campaign({ drops: [drop({ is_claimed: true, current_minutes: 60, percent: 100 })] })],
    }));
    tab("Campaigns");
    const claimed = doc.querySelector("#app .drop.claimed");
    if (!claimed) throw new Error("a claimed drop was not marked");
    if (claimed.querySelector(".bar")) throw new Error("a claimed drop still shows a bar");
  });

  console.log("");
  if (failures.length) {
    console.log(`FAILURES (${failures.length} of ${checks}):`);
    failures.forEach((f) => console.log("  - " + f));
    process.exit(1);
  }
  console.log(`All ${checks} adversarial checks passed.`);
  process.exit(0);
})();
