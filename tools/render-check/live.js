// Drives the real dashboard against a real running miner.
//
// `check.js` renders a captured snapshot and `adversarial.js` attacks the page
// with hostile data, but both stub the network. This one loads the page from
// the miner's own HTTP server, opens a real SSE stream, clicks real controls
// and then asks the miner — over a separate connection — whether it actually
// did what it was told.
//
// It is the only check that covers the whole path: browser to axum to the
// engine and back. Run it against a miner you do not mind having its strategy
// changed and its channel switched.
//
//   node tools/render-check/live.js [http://127.0.0.1:8080] [token]
const { JSDOM } = require("jsdom");

const base = (process.argv[2] || "http://127.0.0.1:8080").replace(/\/$/, "");
const token = process.argv[3] || "";
const q = (path) => base + path + (token ? (path.includes("?") ? "&" : "?") + "token=" + token : "");

const failures = [];
let checks = 0;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/// Waits for the miner to apply a command.
///
/// Commands are applied between passes, and a pass that is gathering
/// candidates takes several seconds against a real account — so this is what
/// a user experiences, not an artificial delay.
async function settle(until, timeout = 25000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    await sleep(500);
    if (!until) continue;
    try {
      if (until(await truth())) return;
    } catch (e) { /* keep waiting */ }
  }
}

async function check(label, fn) {
  checks++;
  try {
    await fn();
    console.log("  ok   " + label);
  } catch (e) {
    failures.push(`${label}: ${e.message}`);
    console.log("  FAIL " + label + " -> " + e.message);
  }
}

/// Asks the miner directly, bypassing the page.
async function truth() {
  const res = await fetch(q("/api/snapshots"));
  if (!res.ok) throw new Error(`the miner returned ${res.status}`);
  const list = await res.json();
  if (!list.length) throw new Error("the miner has not reported yet");
  return list[0];
}

/// A real SSE client, so the page receives what the miner actually pushes.
function makeEventSource(window) {
  return class LiveEventSource {
    constructor(url) {
      this.listeners = {};
      this.onopen = null;
      this.onerror = null;
      window.__es = this;
      this._start(url);
    }
    addEventListener(type, fn) { (this.listeners[type] ||= []).push(fn); }
    close() { this._closed = true; if (this._reader) this._reader.cancel().catch(() => {}); }

    async _start(url) {
      try {
        const absolute = String(url).startsWith("http") ? String(url) : base + String(url);
        const res = await fetch(absolute, { headers: { accept: "text/event-stream" } });
        if (!res.ok) throw new Error("HTTP " + res.status);
        if (this.onopen) this.onopen();
        this._reader = res.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        while (!this._closed) {
          const { value, done } = await this._reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          let idx;
          while ((idx = buffer.indexOf("\n\n")) !== -1) {
            const frame = buffer.slice(0, idx);
            buffer = buffer.slice(idx + 2);
            let type = "message";
            const data = [];
            for (const line of frame.split("\n")) {
              if (line.startsWith("event:")) type = line.slice(6).trim();
              else if (line.startsWith("data:")) data.push(line.slice(5).trim());
            }
            if (!data.length) continue;
            const msg = { data: data.join("\n") };
            (this.listeners[type] || []).forEach((fn) => fn(msg));
          }
        }
      } catch (e) {
        if (this.onerror) this.onerror(e);
      }
    }
  };
}

(async () => {
  console.log(`Driving the live dashboard at ${base}\n`);

  let page;
  try {
    const res = await fetch(q("/"));
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    page = await res.text();
  } catch (e) {
    console.error(
      `Could not load the dashboard at ${base} (${e.message}).\n` +
      `Start one with: btdm mine`,
    );
    process.exit(2);
  }

  const before = await truth();
  console.log(`  (account "${before.account}", strategy ${before.strategy}, state ${before.state})\n`);

  const dom = new JSDOM(page, {
    runScripts: "dangerously",
    url: q("/"),
    pretendToBeVisual: true,
    beforeParse(window) {
      window.addEventListener("error", (e) =>
        failures.push("uncaught page error: " + e.message));
      // The real thing, not a stub. Node's fetch demands an absolute URL,
      // whereas a browser resolves a relative one against the document — the
      // page quite reasonably uses relative paths.
      window.fetch = (url, opts) => {
        const absolute = String(url).startsWith("http") ? String(url) : base + String(url);
        return fetch(absolute, opts);
      };
      window.EventSource = makeEventSource(window);
    },
  });

  const { window } = dom;
  const doc = window.document;
  const click = (n) => n.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  const button = (re) =>
    [...doc.querySelectorAll("button")].find((b) => re.test(b.textContent));
  const tab = (label) => {
    const b = [...doc.querySelectorAll(".tabs button")].find((x) =>
      x.textContent.startsWith(label));
    if (!b) throw new Error(`no tab "${label}"`);
    click(b);
  };

  // Give the page time to load a snapshot over the real stream.
  await sleep(1500);

  console.log("Rendering live state:");

  await check("the page rendered the miner's real state", async () => {
    const t = doc.querySelector("#app").textContent;
    if (t.includes("Waiting for the miner")) throw new Error("still the placeholder");
    if (!t.includes(before.account)) throw new Error("the account is not shown");
  });

  await check("the live SSE stream delivered a snapshot", async () => {
    if (doc.querySelector("#conn-text").textContent !== "live") {
      throw new Error(`connection reads "${doc.querySelector("#conn-text").textContent}"`);
    }
  });

  await check("what is on screen matches what the miner reports", async () => {
    const t = doc.querySelector("#app").textContent;
    if (before.watching) {
      if (!t.includes(before.watching.channel_display_name)) {
        throw new Error("the watched channel is not on screen");
      }
      if (before.watching.current_minutes != null) {
        // The miner is live: Twitch confirms another minute roughly every
        // minute, and a pushed snapshot can land between fetching the
        // reference and reading the page. So the requirement is not the exact
        // number we happened to fetch — it is that the page shows a real
        // confirmed figure for the same drop, and never one *below* it, since
        // confirmed minutes within a session only climb.
        const shown = t.match(/(\d+)\/(\d+) min confirmed/);
        if (!shown) throw new Error("no confirmed-minutes figure is on screen");
        const [, current, required] = shown.map(Number);
        if (required !== before.watching.required_minutes) {
          throw new Error(
            `requirement on screen is ${required}, miner says ${before.watching.required_minutes}`,
          );
        }
        if (current < before.watching.current_minutes) {
          throw new Error(
            `screen shows ${current} confirmed, behind the miner's ${before.watching.current_minutes}`,
          );
        }
      }
    }
    if (!t.includes(String(before.totals.campaigns_eligible))) {
      throw new Error("the campaign count is not on screen");
    }
  });

  await check("every campaign the miner knows about is rendered", async () => {
    tab("Campaigns");
    const cards = doc.querySelectorAll("#app .camp").length;
    if (cards !== before.eligible.length) {
      throw new Error(`${cards} cards for ${before.eligible.length} campaigns`);
    }
  });

  await check("real drop artwork loads from Twitch's CDN", async () => {
    const imgs = [...doc.querySelectorAll("#app img")];
    if (!imgs.length) throw new Error("no artwork at all");
    const bad = imgs.filter((i) => !/^https:\/\/static-cdn\.jtvnw\.net\//.test(i.src));
    if (bad.length) throw new Error(`${bad.length} images from an unexpected host`);
    // Prove one actually exists rather than 404s.
    const res = await fetch(imgs[0].src, { method: "HEAD" });
    if (!res.ok) throw new Error(`artwork 404s: ${imgs[0].src}`);
  });

  console.log("\nControls, verified against the miner:");

  // Pausing is persisted, so a previous run may have left it paused. Start
  // from a known state rather than assuming one.
  if (before.paused) {
    click(button(/Resume mining/));
    await settle((s) => !s.paused);
  }

  const original = before.strategy;
  const other = before.strategies.find((s) => !s.selected).name;

  await check(`changing the strategy to ${other} takes effect`, async () => {
    const sel = doc.querySelector("#app select");
    if (!sel) throw new Error("no strategy picker");
    sel.value = other;
    sel.dispatchEvent(new window.Event("change"));
    await settle((s) => s.strategy === other);
    const now = await truth();
    if (now.strategy !== other) {
      throw new Error(`the miner still reports ${now.strategy}`);
    }
  });

  await check("the change came back over the stream without a reload", async () => {
    const t = doc.querySelector("#app").textContent;
    if (!t.includes(other)) throw new Error("the page still shows the old strategy");
  });

  await check("pausing actually stops the miner", async () => {
    click(button(/Pause mining/));
    await settle((s) => s.paused);
    const now = await truth();
    if (!now.paused) throw new Error("the miner is still running");
    if (now.watching) throw new Error("it is paused but still watching something");
  });

  await check("the paused state reaches the page", async () => {
    const t = doc.querySelector("#app").textContent;
    if (!t.includes("Paused")) throw new Error("the page does not show it as paused");
  });

  await check("resuming starts it again", async () => {
    click(button(/Resume mining/));
    await settle((s) => !s.paused);
    const now = await truth();
    if (now.paused) throw new Error("the miner is still paused");
  });

  await check("excluding a campaign marks it, and re-including clears it", async () => {
    tab("Campaigns");
    const card = doc.querySelector("#app .camp");
    if (!card) throw new Error("no campaign to exclude");
    const name = card.querySelector("h3").textContent;
    click([...card.querySelectorAll("button")].find((b) => b.textContent === "Exclude"));
    await settle((s) => s.settings.excluded_campaigns.length > 0);

    let now = await truth();
    const excluded = now.eligible.find((c) => c.name === name);
    if (excluded && !excluded.excluded) throw new Error("the miner did not exclude it");

    // Put it back, so this test leaves nothing behind.
    const target = now.settings.excluded_campaigns[0];
    await fetch(q(`/api/accounts/${encodeURIComponent(now.account)}/command`), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        command: "set_campaign_excluded",
        campaign: target,
        excluded: false,
      }),
    });
    await settle((s) => s.settings.excluded_campaigns.length === 0);
    now = await truth();
    if (now.settings.excluded_campaigns.length) {
      throw new Error("the exclusion was not undone");
    }
  });

  await check("a favourite added from the live list is remembered", async () => {
    tab("Live now");
    const star = [...doc.querySelectorAll("#app button")].find((b) => b.textContent === "☆");
    if (!star) throw new Error("no channel to favourite");
    click(star);
    await settle((s) => s.settings.favourites.length > 0);
    const now = await truth();
    if (!now.settings.favourites.length) throw new Error("the miner did not record it");

    // Undo.
    await fetch(q(`/api/accounts/${encodeURIComponent(now.account)}/command`), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        command: "set_favourite",
        login: now.settings.favourites[0],
        on: false,
      }),
    });
    await settle();
  });

  await check("the activity log shows what really happened", async () => {
    tab("Activity");
    const lines = doc.querySelectorAll("#log .line").length;
    if (!lines) throw new Error("the log is empty despite a running miner");
    const t = doc.querySelector("#log").textContent;
    if (/[{}]|Id\(/.test(t)) throw new Error("debug output leaked into the log");
  });

  await check("the rewards tab reflects the stored claim history", async () => {
    tab("Rewards");
    // It is fetched, not pushed.
    await sleep(1500);
    const stored = await (await fetch(q(`/api/rewards?account=${encodeURIComponent(before.account)}`))).json();
    const t = doc.querySelector("#app").textContent;
    if (stored.length && !t.includes(stored[0].name)) {
      throw new Error(`"${stored[0].name}" was claimed but is not shown`);
    }
    if (!stored.length && !/Nothing claimed yet/.test(t)) {
      throw new Error("nothing claimed, but the page does not say so");
    }
  });

  await check("the health probe agrees with the page", async () => {
    const health = await (await fetch(base + "/api/health")).json();
    const now = await truth();
    const mining = now.state === "Mining";
    if (mining && health.status !== "mining") {
      throw new Error(`page says mining, probe says ${health.status}`);
    }
  });

  console.log("\nPutting it back:");

  await check(`the strategy is restored to ${original}`, async () => {
    await fetch(q(`/api/accounts/${encodeURIComponent(before.account)}/command`), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ command: "set_strategy", name: original }),
    });
    await settle((s) => s.strategy === original);
    const now = await truth();
    if (now.strategy !== original) throw new Error(`left it on ${now.strategy}`);
  });

  console.log("");
  if (failures.length) {
    console.log(`FAILURES (${failures.length} of ${checks}):`);
    failures.forEach((f) => console.log("  - " + f));
    process.exit(1);
  }
  console.log(`All ${checks} live checks passed against a real miner.`);
  process.exit(0);
})();
