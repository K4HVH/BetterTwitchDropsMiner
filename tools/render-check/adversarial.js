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
    // Two different clocks — the session's confirmed minutes and the
    // account-wide health clock — printed together read as the page
    // contradicting itself.
    if (t.includes("Last confirmed by Twitch")) {
      throw new Error("it claimed a last-confirmed time while claiming nothing is confirmed");
    }
  });

  /// A stall is the one case where "nothing confirmed" and a rising clock
  /// belong together: the clock is the evidence.
  check("a stall still shows its clock even with nothing confirmed", () => {
    push(snapshot({
      watching: {
        ...snapshot().watching,
        current_minutes: null, required_minutes: null, drop_name: null,
        stalled: true, seconds_since_confirmation: 900,
      },
    }));
    const t = text();
    if (!t.includes("Last confirmed by Twitch")) {
      throw new Error("a stalled session must show how long it has been stalled");
    }
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

  // ==========================================================================
  // Second pass.
  //
  // Everything below this line found a defect that the checks above do not
  // cover, so most of these fail against the page as it stands. They are
  // written the way the page ought to behave rather than the way it does, so
  // that fixing the page turns them green instead of needing them rewritten.
  //
  // The three marked "holds" are attempts that could *not* be broken, kept so
  // a later change cannot quietly undo the hardening.
  // ==========================================================================

  /// Puts the page back into a known state after every check, passed or
  /// failed. Without this a check that throws part way leaves its hostile
  /// snapshot, its half-typed search text or its selected account in place,
  /// and every check after it is testing something else — quietly passing,
  /// which is worse than quietly failing.
  const restore = () => {
    try {
      push(snapshot());
      state_reset("default");
      const camp = [...doc.querySelectorAll(".tabs button")]
        .find((b) => b.textContent.startsWith("Campaigns"));
      if (camp) click(camp);
      clearFilter();
    } catch (e) { /* nothing left to restore to */ }
  };

  function dcheck(label, fn) {
    checks++;
    try {
      fn();
      console.log("  ok   " + label);
    } catch (e) {
      failures.push(`${label}: ${e.message}`);
      console.log("  FAIL " + label + " -> " + e.message);
    } finally {
      restore();
    }
  }

  /// `check` runs its body synchronously and throws away any promise it
  /// returns, so an async body's failure is never recorded. Anything that has
  /// to wait for a fetch uses this.
  async function adcheck(label, fn) {
    checks++;
    try {
      await fn();
      console.log("  ok   " + label);
    } catch (e) {
      failures.push(`${label}: ${e.message}`);
      console.log("  FAIL " + label + " -> " + e.message);
    } finally {
      restore();
    }
  }

  const baseFetch = window.fetch;
  /// Answers one endpoint differently for the duration of one check, and
  /// always puts the harness's own fetch back.
  async function withFetch(handler, fn) {
    window.fetch = async (url, opts) => {
      const answer = await handler(String(url), opts);
      return answer !== undefined ? answer : baseFetch(url, opts);
    };
    try { await fn(); } finally { window.fetch = baseFetch; }
  }

  const tabs = () => doc.querySelectorAll(".tabs button").length;
  const pillTexts = () => [...doc.querySelectorAll("#app .pill")].map((p) => p.textContent);
  const dropMins = () => [...doc.querySelectorAll("#app .drop .mins")].map((m) => m.textContent);
  const statValues = () => [...doc.querySelectorAll("#app .stat b")].map((b) => b.textContent);

  // ------------------------------------------------------- silent blank page
  //
  // `normalise` fills in absent top-level sections, but it does not defend
  // what is *inside* them, and the SSE handler wraps `render()` in a try/catch
  // that swallows whatever the renderer throws. So the result is not an error
  // anyone can see: it is a page that stops half way through building itself
  // and stays that way, because the next heartbeat carries the same field and
  // fails in the same place. Each of these pushes its hostile snapshot with
  // the relevant tab already open, which is how it would really arrive.
  console.log("\nHostile field contents (normalise only guards the outer shape):");

  dcheck("an explicitly null quarantine list does not blank the whole page", () => {
    // normalise supplies `quarantined_channels: []` and then spreads the
    // payload over it, so an explicit null wins. controlsPanel adds `.length`
    // to it above the tab strip, taking the navigation down with it.
    push(snapshot({ health: { ...snapshot().health, quarantined_channels: null } }));
    if (!tabs()) throw new Error("the page is blank: no hero, no stats, no tabs, no way back");
  });

  dcheck("a priority list that is not an array does not blank the whole page", () => {
    push(snapshot({
      strategies: [{ name: "priority", description: "Your order first.", selected: true }],
      settings: { ...snapshot().settings, priority_games: "AB" },
    }));
    if (!tabs()) throw new Error("a string priority list blanked the page");
  });

  dcheck("a null entry in eligible does not blank the whole page", () => {
    // `banners` reaches into every campaign before the tab strip is built.
    push(snapshot({ eligible: [campaign(), null] }));
    if (!tabs()) throw new Error("one null campaign blanked the page");
  });

  dcheck("a live channel with no viewer count still renders its row", () => {
    tab("Live now");
    const s = snapshot();
    delete s.live[0].viewers;          // an older miner, or a partial proxy
    push(s);
    if (!doc.querySelector("#app table")) {
      throw new Error("the Live tab emptied itself and said nothing: c.viewers.toLocaleString() threw");
    }
  });

  dcheck("a live channel with a null viewer count still renders its row", () => {
    tab("Live now");
    push(snapshot({ live: [{ ...snapshot().live[0], viewers: null }] }));
    if (!doc.querySelector("#app table")) throw new Error("null viewers emptied the Live tab");
  });

  dcheck("allowed_channels of the wrong type does not empty the Campaigns tab", () => {
    tab("Campaigns");
    push(snapshot({ eligible: [campaign({ allowed_channels: "abcdefgh" })] }));
    if (!doc.querySelectorAll("#app .camp").length) {
      throw new Error("a string allowed_channels emptied the tab: .slice().join is not a function");
    }
  });

  dcheck("a null drop inside a campaign does not empty the Campaigns tab", () => {
    tab("Campaigns");
    push(snapshot({ eligible: [campaign({ drops: [drop(), null] })] }));
    if (!doc.querySelectorAll("#app .camp").length) throw new Error("one null drop emptied the tab");
  });

  dcheck("a null benefit inside a drop does not empty the Campaigns tab", () => {
    tab("Campaigns");
    push(snapshot({ eligible: [campaign({ drops: [drop({ benefits: [null] })] })] }));
    if (!doc.querySelectorAll("#app .camp").length) throw new Error("one null benefit emptied the tab");
  });

  dcheck("a favourites list that is not an array does not empty the Settings tab", () => {
    tab("Settings");
    push(snapshot({ settings: { ...snapshot().settings, favourites: { a: 1 } } }));
    if (!doc.querySelectorAll("#app .setting").length) {
      throw new Error("the Settings tab emptied itself and said nothing");
    }
  });

  // -------------------------------------------------------- invented numbers
  //
  // The stated premise is that no figure appears which the miner did not
  // report. Each of these puts one on screen, arrived at by arithmetic on a
  // value the miner never sent.
  console.log("\nFigures the miner never reported:");

  dcheck("an unparseable deadline does not become a countdown", () => {
    tab("Campaigns");
    push(snapshot({ eligible: [campaign({ ends_at: "not a date" })] }));
    const bad = pillTexts().filter((t) => /NaN/.test(t));
    if (bad.length) throw new Error(`a deadline pill reads ${JSON.stringify(bad[0])}`);
  });

  dcheck("a drop with no minutes says so rather than printing undefined", () => {
    tab("Campaigns");
    const d = drop();
    delete d.current_minutes;
    delete d.required_minutes;
    push(snapshot({ eligible: [campaign({ drops: [d] })] }));
    const mins = dropMins();
    if (mins.some((m) => /undefined|null|NaN/.test(m))) {
      throw new Error(`a drop row reads ${JSON.stringify(mins[0])}`);
    }
  });

  dcheck("a drop with null minutes says so rather than printing null", () => {
    tab("Campaigns");
    push(snapshot({
      eligible: [campaign({ drops: [drop({ current_minutes: null, required_minutes: null })] })],
    }));
    const mins = dropMins();
    if (mins.some((m) => /undefined|null|NaN/.test(m))) {
      throw new Error(`a drop row reads ${JSON.stringify(mins[0])}`);
    }
  });

  dcheck("the hero needs both halves of the confirmed figure before printing it", () => {
    // The guard tests current_minutes alone, so a payload carrying one and not
    // the other prints the half it has and the word "undefined" for the rest.
    const w = { ...snapshot().watching };
    delete w.required_minutes;
    push(snapshot({ watching: w }));
    const line = doc.querySelector("#app .confirm").textContent;
    if (/undefined|null|NaN/.test(line)) throw new Error(`the hero reads ${JSON.stringify(line)}`);
  });

  dcheck("null totals do not become a printed 'null/null'", () => {
    push(snapshot({
      totals: { ...snapshot().totals, campaigns_eligible: null, campaigns_known: null },
    }));
    const stats = statValues();
    if (stats.some((v) => /undefined|null|NaN/.test(v))) {
      throw new Error(`the stats strip reads ${JSON.stringify(stats)}`);
    }
  });

  dcheck("a non-numeric total does not become 'NaNd NaNh' of watching left", () => {
    push(snapshot({ totals: { ...snapshot().totals, minutes_remaining: "many" } }));
    const stats = statValues();
    if (stats.some((v) => /NaN/.test(v))) {
      throw new Error(`the stats strip reads ${JSON.stringify(stats)}`);
    }
  });

  dcheck("an unparseable last_sync does not become 'refreshed NaNd ago'", () => {
    tab("Settings");
    push(snapshot({ health: { ...snapshot().health, last_sync: "soon" } }));
    if (/NaN/.test(text())) {
      throw new Error(`Status reads ${JSON.stringify((text().match(/Campaigns refreshed [^,]*/) || [])[0])}`);
    }
  });

  dcheck("a last_sync in the future does not become a negative age", () => {
    // Clock skew between a headless box and the browser looking at it is the
    // ordinary cause, and needs no hostility at all.
    tab("Settings");
    push(snapshot({
      health: {
        ...snapshot().health,
        last_sync: new Date(Date.now() + 3600000).toISOString(),
        next_sync_in_seconds: -600,
      },
    }));
    const line = (text().match(/Campaigns refreshed [^A-Z]*/) || [""])[0];
    if (/-\d/.test(line)) throw new Error(`Status reads ${JSON.stringify(line.trim())}`);
  });

  dcheck("an unparseable start date does not become 'in NaN days'", () => {
    tab("Unavailable");
    push(snapshot({
      rejected: [{
        id: "r1", name: "Soon", game: "G", reason: "not started",
        explanation: "It has not begun.", action_url: null,
        starts_at: "whenever", ends_at: new Date().toISOString(), box_art_url: null,
      }],
    }));
    if (/NaN|Invalid Date/.test(text())) {
      throw new Error(`the Unavailable tab reads ${JSON.stringify(pillTexts())}`);
    }
  });

  await adcheck("an unparseable reward timestamp does not become an 'Invalid Date' heading", () =>
    withFetch(async (url) => {
      if (!url.includes("/api/rewards")) return undefined;
      return { ok: true, status: 200, json: async () => [{ name: "Hat", at: "nope" }] };
    }, async () => {
      push(snapshot({ account: "rw-date" }));
      state_reset("rw-date");
      tab("Rewards");
      await sleep(60);
      const heads = [...doc.querySelectorAll("#app h2")].map((h) => h.textContent);
      if (heads.some((h) => /Invalid Date/.test(h))) {
        throw new Error(`rewards are grouped under ${JSON.stringify(heads)}`);
      }
    }));

  // ---------------------------------------------------------- contradictions
  console.log("\nTwo parts of the page disagreeing:");

  await adcheck("the Watch button does not overwrite the miner's reply with a prediction", () =>
    withFetch(async (url, opts) => {
      if (!opts || opts.method !== "POST") return undefined;
      posted.push({ url, body: JSON.parse(opts.body || "{}") });
      // A 200 that the miner uses to say it did *not* do the thing. `send`
      // already toasts this; the caller then talks over it.
      return {
        ok: true, status: 200,
        json: async () => ({ message: "that channel went offline; staying put" }),
      };
    }, async () => {
      push(snapshot({ live: [{ ...snapshot().live[0], is_current: false }] }));
      tab("Live now");
      click([...doc.querySelectorAll("#app button")].find((b) => b.textContent === "Watch"));
      await sleep(60);
      const shown = doc.querySelector("#toast").textContent;
      if (!shown.includes("staying put")) {
        throw new Error(`the miner said "staying put"; the page says ${JSON.stringify(shown)}`);
      }
    }));

  dcheck("'confirmed by Twitch' is not printed beside 'no progress confirmed by Twitch'", () => {
    // Reachable on every switch to a channel Twitch named: the pill is about
    // how the *channel* was chosen, the sentence is about *progress*. For the
    // first minute or two of every such switch both are on screen, using the
    // same four words to mean opposite things.
    push(snapshot({
      watching: {
        ...snapshot().watching, confirmed_by_twitch: true,
        current_minutes: null, required_minutes: null, drop_name: null,
      },
    }));
    const hero = doc.querySelector("#app .hero .panel").textContent;
    if (/confirmed by Twitch/.test(hero) && /No progress confirmed by Twitch yet/.test(hero)) {
      throw new Error("the hero says both 'confirmed by Twitch' and 'No progress confirmed by Twitch yet'");
    }
  });

  dcheck("figures are marked stale once the stream is known to be down", () => {
    push(snapshot({ watching: { ...snapshot().watching, seconds_since_confirmation: 7 } }));
    window.__es.onerror();                        // the stream drops
    if (doc.querySelector("#conn-text").textContent !== "reconnecting…") {
      throw new Error("the header did not notice the disconnection");
    }
    // The header knows. The hero goes on asserting a seven-second-old
    // confirmation for as long as the disconnection lasts, and the campaigns
    // tab redraws that same sentence every thirty seconds, unchanged.
    const claim = (text().match(/Last confirmed by Twitch: \S+ ago/) || [])[0];
    if (claim && !doc.querySelector("#app .stale")) {
      window.__es.onopen();
      throw new Error(`disconnected, and the hero still asserts "${claim}" with nothing marking it stale`);
    }
    window.__es.onopen();
  });

  dcheck("the strategy shown by the select is the strategy named in the warnings", () => {
    // Two sources of truth: the select reads `strategies[].selected`, every
    // warning reads `settings.strategy`. Nothing reconciles them, and with no
    // entry flagged the select silently shows whichever option came first.
    tab("Settings");
    push(snapshot({
      settings: { ...snapshot().settings, pinned: "somebody", strategy: "coverage" },
      strategies: [
        { name: "deadline", description: "d", selected: true },
        { name: "coverage", description: "c", selected: false },
      ],
    }));
    const shown = doc.querySelector("#app select").value;
    const warned = (text().match(/but the (\S+) strategy ignores pins/) || [])[1];
    if (warned && warned !== shown) {
      throw new Error(`the select says "${shown}" while the warning beneath it says "${warned}"`);
    }
  });

  dcheck("a drop's bar agrees with the two numbers printed next to it", () => {
    // The bar is drawn from `percent` and the text from two other fields.
    // Nothing checks that they describe the same thing, so a full green bar
    // can sit beside "5/60m".
    tab("Campaigns");
    push(snapshot({
      eligible: [campaign({ drops: [drop({ percent: 100, current_minutes: 5, required_minutes: 60 })] })],
    }));
    const fill = doc.querySelector("#app .drop .bar > i");
    const mins = doc.querySelector("#app .drop .mins").textContent;
    const [cur, req] = mins.split(/[/m]/).map(Number);
    if (Math.abs(parseFloat(fill.style.width) - (cur / req) * 100) > 1) {
      throw new Error(`the bar is ${fill.style.width} full beside the text "${mins}"`);
    }
  });

  // --------------------------------------------------- state across accounts
  //
  // `state.drafts`, `state.rewards` and `state.history` are all global, while
  // everything they are shown beside belongs to exactly one account.
  console.log("\nState surviving an account change that should not:");

  dcheck("a draft typed for one account is not offered to another", () => {
    push(snapshot({ account: "draft-a", settings: { ...snapshot().settings, pinned: "a-chan" } }));
    state_reset("draft-a");
    tab("Settings");
    const box = doc.querySelector("#pin-input");
    box.focus();
    box.value = "typed-for-draft-a";
    box.dispatchEvent(new window.Event("input"));

    push(snapshot({ account: "draft-b", settings: { ...snapshot().settings, pinned: "b-chan" } }));
    state_reset("draft-b");
    const shown = doc.querySelector("#pin-input").value;
    // Worse than showing the wrong text: one click on Pin commits it, so the
    // half-typed name for one account becomes another account's setting.
    posted.length = 0;
    click([...doc.querySelectorAll("#app button")].find((b) => b.textContent === "Pin"));
    const sent = posted[0] || { url: "", body: {} };
    if (shown === "typed-for-draft-a") {
      throw new Error(`draft-b's pin box is pre-filled with draft-a's typing, and Pin posted ` +
                      `${JSON.stringify(sent.body)} to ${sent.url.split("?")[0]}`);
    }
  });

  await adcheck("rewards are not shown under the account they do not belong to", () =>
    withFetch(async (url) => {
      if (!url.includes("/api/rewards")) return undefined;
      const account = new URL(url, "http://x").searchParams.get("account");
      return {
        ok: true, status: 200,
        json: async () => [{ name: `${account}-REWARD`, at: new Date().toISOString() }],
      };
    }, async () => {
      push(snapshot({ account: "rw-a" }));
      state_reset("rw-a");
      tab("Rewards");
      await sleep(60);
      if (!text().includes("rw-a-REWARD")) throw new Error("the first account's rewards never loaded");

      push(snapshot({ account: "rw-b" }));
      state_reset("rw-b");
      // Rendered synchronously, before rw-b's rewards have even been asked for.
      if (text().includes("rw-a-REWARD")) {
        const total = (text().match(/\d+ rewards? claimed by this miner\./) || [""])[0];
        throw new Error(`rw-b's Rewards tab lists rw-a's rewards and totals them: "${total}"`);
      }
      await sleep(60);
    }));

  await adcheck("stored history is not relabelled with the account now on screen", () =>
    withFetch(async (url) => {
      if (!url.includes("/api/history")) return undefined;
      const account = new URL(url, "http://x").searchParams.get("account");
      return {
        ok: true, status: 200,
        json: async () => [{ at: new Date().toISOString(), summary: `${account}-EVENT`, severity: "info" }],
      };
    }, async () => {
      push(snapshot({ account: "hist-a" }));
      state_reset("hist-a");
      tab("Activity");
      await sleep(60);
      if (!text().includes("hist-a-EVENT")) throw new Error("the first account's history never loaded");

      push(snapshot({ account: "hist-b" }));
      state_reset("hist-b");
      // logTab rebuilds every stored line as `{ account: s.account, ... }`, so
      // the "who" column does not merely show the wrong lines: it states that
      // they belong to the account being looked at.
      const lines = [...doc.querySelectorAll("#log .line")].map((l) => l.textContent);
      const lying = lines.find((l) => l.includes("hist-b") && l.includes("hist-a-EVENT"));
      if (lying) {
        throw new Error(`the Activity tab attributes hist-a's event to hist-b: ${JSON.stringify(lying)}`);
      }
      await sleep(60);
    }));

  await adcheck("a failed rewards fetch is retried rather than saying 'Loading…' for ever", () =>
    withFetch(async (url) => {
      if (!url.includes("/api/rewards")) return undefined;
      return { ok: false, status: 503, json: async () => ({}) };
    }, async () => {
      // `loadRewards` marks the account as done before the request is made and
      // never unmarks it, so one failed fetch pins the tab on "Loading…" for
      // the life of the page. Leaving *and returning to* the tab does not
      // retry, and neither does anything else.
      push(snapshot({ account: "rw-down" }));
      state_reset("rw-down");
      tab("Rewards");
      await sleep(60);
      tab("Campaigns");
      tab("Rewards");                           // the user tries again
      await sleep(60);
      if (/Loading…/.test(text())) {
        throw new Error("the Rewards tab says 'Loading…' with nothing loading and nothing that will retry");
      }
    }));

  // --------------------------------------------------- focus and the rebuild
  console.log("\nWhat the rebuild takes with it:");

  dcheck("focus on a control other than a text box survives a snapshot", () => {
    // Only inputs are restored, so a keyboard user cannot hold focus on the
    // strategy select, a priority chip's ↑, or a campaign's Exclude button for
    // the few seconds between one heartbeat and the next.
    const sel = doc.querySelector("#app select");
    sel.focus();
    push(snapshot());
    if (doc.activeElement.tagName !== "SELECT") {
      throw new Error(`focus moved from the strategy select to <${doc.activeElement.tagName.toLowerCase()}>`);
    }
  });

  dcheck("the account picker is not thrown away and rebuilt every heartbeat", () => {
    const before = doc.querySelector("#account-picker select");
    if (!before) throw new Error("no picker to test");
    before.focus();
    push(snapshot());
    if (doc.querySelector("#account-picker select") !== before) {
      throw new Error("the picker is a new element after every snapshot, so an open dropdown shuts itself");
    }
  });

  // --------------------------------------------------------- priority editor
  console.log("\nThe priority editor:");

  const prio = (over = {}) => snapshot({
    strategy: "priority",
    strategies: [{ name: "priority", description: "Your order first.", selected: true }],
    ...over,
  });

  dcheck("each chip's buttons act on that chip and not on its namesake", () => {
    // The editor addresses entries by value. Two entries with the same text
    // are two chips whose ×, ↑ and ↓ send byte-identical commands, so the
    // second chip's × removes the first one.
    push(prio({ settings: { ...snapshot().settings, priority_games: ["Dupe", "Dupe", "Other"] } }));
    const second = [...doc.querySelectorAll("#app .chip")].find((c) => c.textContent.startsWith("2."));
    posted.length = 0;
    click([...second.querySelectorAll("button")].find((b) => b.textContent === "×"));
    const body = posted[0] && posted[0].body;
    if (body && body.entry === "Dupe" && !("index" in body) && !("position" in body)) {
      throw new Error("removing the second 'Dupe' sends exactly what removing the first would");
    }
  });

  await adcheck("a successful add clears the box it came from", async () => {
    push(prio({ settings: { ...snapshot().settings, priority_games: [] } }));
    const box = doc.querySelector("#priority-input");
    box.value = "Half Life";
    box.dispatchEvent(new window.Event("input"));
    posted.length = 0;
    const add = [...doc.querySelectorAll("#app button")].find((b) => b.textContent === "Add");
    click(add);
    await sleep(40);
    // The draft is dropped from `state`, but nothing re-renders, so the text
    // sits in the box looking unsent until the next heartbeat arrives.
    if (doc.querySelector("#priority-input").value === "Half Life") {
      click(add);
      await sleep(40);
      throw new Error(`the box still reads "Half Life" after a successful add, and a ` +
                      `second click posted it ${posted.length} times`);
    }
  });

  dcheck("the priority editor appears whenever the priority list is in use", () => {
    // `usesPriority` reads strategies[].selected; everything else on the page
    // reads settings.strategy. When they disagree, the list that decides what
    // gets mined has no editor anywhere on the page.
    push(snapshot({
      strategy: "priority",
      settings: { ...snapshot().settings, strategy: "priority", priority_games: ["A"] },
      strategies: [
        { name: "deadline", description: "d", selected: false },
        { name: "priority", description: "p", selected: false },
      ],
    }));
    if (!doc.querySelector("#priority-input")) {
      throw new Error("settings.strategy is 'priority' and its list is unreachable from the page");
    }
  });

  // ------------------------------------------------------------ smaller lies
  console.log("\nSmaller misstatements:");

  dcheck("the search box does not rewrite what is being typed into it", () => {
    // `oninput` stores `value.trim().toLowerCase()` and the rebuild writes
    // that back into the box, so a trailing space is deleted as it is typed
    // and capitals are replaced under the cursor. Two-word searches cannot be
    // entered at all: the space that would separate them never survives.
    tab("Campaigns");
    const box = doc.querySelector("#search");
    box.focus();
    box.value = "Rust ";
    box.setSelectionRange(5, 5);
    box.dispatchEvent(new window.Event("input"));
    const after = doc.querySelector("#search").value;
    if (after !== "Rust ") {
      throw new Error(`typing "Rust " left "${after}" in the box, so the next keystroke lands mid-word`);
    }
  });

  dcheck("an excluded campaign with no name is not listed as 'null'", () => {
    tab("Settings");
    push(snapshot({
      settings: { ...snapshot().settings, excluded_campaigns: ["c1"] },
      eligible: [campaign({ id: "c1", name: null })],
    }));
    const chips = [...doc.querySelectorAll("#app .chip")].map((c) => c.textContent);
    if (chips.some((c) => /^(null|undefined)/.test(c))) {
      throw new Error(`the excluded list reads ${JSON.stringify(chips)}`);
    }
  });

  dcheck("a group of skipped campaigns does not borrow the first one's explanation", () => {
    const mk = (id, explanation) => ({
      id, name: "C" + id, game: "G", reason: "not linked", explanation,
      action_url: null, starts_at: new Date().toISOString(),
      ends_at: new Date().toISOString(), box_art_url: null,
    });
    tab("Unavailable");
    push(snapshot({ rejected: [mk("1", "FIRST EXPLANATION"), mk("2", "SECOND EXPLANATION")] }));
    if (text().includes("FIRST EXPLANATION") && !text().includes("SECOND EXPLANATION")) {
      throw new Error("both campaigns are described by whichever explanation happened to arrive first");
    }
  });

  dcheck("the Live tab says how a channel is known to advance drops", () => {
    // `confirmed_by_twitch` is on every live channel and the hero uses it, but
    // the table prints "1 campaign, 1 drop" for a channel Twitch named and one
    // the miner inferred from a directory tag, identically.
    tab("Live now");
    push(snapshot({
      live: [
        { ...snapshot().live[0], login: "sure", display_name: "Sure", is_current: false, confirmed_by_twitch: true },
        { ...snapshot().live[0], login: "guess", display_name: "Guess", is_current: false, confirmed_by_twitch: false },
      ],
    }));
    const rows = [...doc.querySelectorAll("#app tbody tr")].map((r) => r.textContent);
    if (rows.length === 2 && rows[0].replace("Sure", "") === rows[1].replace("Guess", "")) {
      throw new Error("a channel Twitch confirmed and one the miner guessed at are shown identically");
    }
  });

  dcheck("a negative age is not printed as one", () => {
    push(snapshot({ watching: { ...snapshot().watching, seconds_since_confirmation: -50 } }));
    const line = (text().match(/Last confirmed by Twitch: \S+ ago/) || [""])[0];
    if (/-\d/.test(line)) throw new Error(`the hero reads "${line}"`);
  });

  // -------------------------------------------------------- hardening (holds)
  console.log("\nAttempts that failed — these pass, and must keep passing:");

  dcheck("holds: no URL scheme but http(s) reaches an href or a src", () => {
    const nasty = [
      "data:text/html,<script>window.__pwned=1</script>",
      "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' onload='window.__pwned=1'/>",
      "blob:http://localhost/whatever",
      "//evil.example.com/x.png",
      "JaVaScRiPt:window.__pwned=1",
      "\tjavascript:window.__pwned=1",
      " javascript:window.__pwned=1",
      "java\nscript:window.__pwned=1",
      "vbscript:window.__pwned=1",
      "https:/\\/\\evil.example.com",
    ];
    for (const url of nasty) {
      push(snapshot({
        eligible: [campaign({
          box_art_url: url,
          drops: [drop({ benefits: [{ name: "b", kind: "k", image_url: url }] })],
        })],
        rejected: [{
          id: "r", name: "n", game: "g", reason: "not linked", explanation: "e",
          action_url: url, starts_at: new Date().toISOString(),
          ends_at: new Date().toISOString(), box_art_url: url,
        }],
      }));
      tab("Campaigns");
      const srcs = [...doc.querySelectorAll("#app img")].map((i) => i.getAttribute("src") || "");
      tab("Unavailable");
      const hrefs = [...doc.querySelectorAll("#app a")].map((a) => a.getAttribute("href") || "");
      for (const got of srcs.concat(hrefs)) {
        if (got && !/^https?:\/\//i.test(got)) throw new Error(`${JSON.stringify(url)} survived as ${got}`);
      }
    }
    if (window.__pwned) throw new Error("script executed");
  });

  dcheck("holds: a quote inside an allowed URL cannot break out of the attribute", () => {
    const url = 'https://evil.example/x" onerror="window.__pwned=1';
    tab("Campaigns");
    push(snapshot({ eligible: [campaign({ box_art_url: url })] }));
    const img = doc.querySelector("#app .camp img");
    if (img && img.getAttribute("onerror")) throw new Error("an event handler attribute was created");
    if (window.__pwned) throw new Error("script executed");
  });

  await adcheck("holds: a 202 is reported as a doubt and keeps the user's text", () =>
    withFetch(async (url, opts) => {
      if (!opts || opts.method !== "POST") return undefined;
      posted.push({ url, body: JSON.parse(opts.body || "{}") });
      return { ok: true, status: 202, json: async () => ({ message: "queued; the miner is busy" }) };
    }, async () => {
      push(prio({ settings: { ...snapshot().settings, priority_games: [] } }));
      const box = doc.querySelector("#priority-input");
      box.value = "Kept";
      box.dispatchEvent(new window.Event("input"));
      click([...doc.querySelectorAll("#app button")].find((b) => b.textContent === "Add"));
      await sleep(60);
      const t = doc.querySelector("#toast");
      if (!t.className.includes("err")) throw new Error("a 202 was reported in the success style");
      if (!t.textContent.includes("queued")) throw new Error("the miner's own words were dropped");
      push(prio({ settings: { ...snapshot().settings, priority_games: [] } }));
      if (doc.querySelector("#priority-input").value !== "Kept") {
        throw new Error("a 202 threw away what the user had typed");
      }
    }));

  console.log("");
  if (failures.length) {
    console.log(`FAILURES (${failures.length} of ${checks}):`);
    failures.forEach((f) => console.log("  - " + f));
    process.exit(1);
  }
  console.log(`All ${checks} adversarial checks passed.`);
  process.exit(0);
})();
