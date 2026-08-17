#!/usr/bin/env python3
"""Check the dashboard's claims against Twitch's own answers.

`tools/render-check/` proves the page renders what the miner sends it. Nothing
proved that what the miner sends is *true*. That gap is where the worst bugs
have lived, because they are invisible from inside: the miner reports a
campaign as unclaimed and 0 of 180 minutes, the page faithfully draws it, every
test passes, and the account has owned the reward for six days.

So this asks Twitch directly, for every campaign the dashboard currently calls
mineable, and compares field by field:

  * status, window, game and tier list
  * the allow-list, by membership and by size
  * each tier's required minutes
  * each tier's claimed state, cross-checked three ways — the campaign's own
    `self.isClaimed`, the in-progress inventory, and the `gameEventDrops`
    awarded-benefit ledger
  * each tier's progress against the inventory's figure
  * the arithmetic each card asserts about itself: slack against the deadline,
    the at-risk flag against slack, and the totals against the rows

Usage:

    python3 tools/audit-live/audit.py [dashboard-url] [--data-dir DIR]

Requires a running miner and a linked account. Read-only: it issues queries and
a single inventory read, and changes nothing.

Security: this reads the OAuth token out of `state.db` to talk to Twitch as the
account. It never prints it, and it opens the database read-only. Treat any
terminal capture of a run as you would the database itself.
"""
import argparse
import datetime
import json
import os
import sqlite3
import sys
import urllib.request

CLIENT_ID = "kd1unb4b3q4t58fwlpcbzcbnm76a8fp"
USER_AGENT = (
    "Dalvik/2.1.0 (Linux; U; Android 16; SM-S911B Build/TP1A.220624.014) "
    "tv.twitch.android.app/25.3.0/2503006"
)
GQL_URL = "https://gql.twitch.tv/gql"

INVENTORY = """query Inventory { currentUser { inventory {
  dropCampaignsInProgress { id timeBasedDrops { id self { currentMinutesWatched isClaimed } } }
  gameEventDrops { id lastAwardedAt } } } }"""

DETAIL = """query DropCampaignDetails($dropID: ID!, $channelLogin: String!) {
  user(login: $channelLogin) { dropCampaign(id: $dropID) {
    id name status startAt endAt game { id name slug }
    allow { channels { id name displayName } }
    timeBasedDrops { id name requiredMinutesWatched startAt endAt
      self { currentMinutesWatched isClaimed }
      benefitEdges { benefit { id } } } } } }"""

# Mirrors AT_RISK_SLACK_MINUTES in crates/btdm-core/src/snapshot.rs.
AT_RISK_SLACK_MINUTES = 120


def when(text):
    return datetime.datetime.fromisoformat(text.replace("Z", "+00:00"))


def default_data_dir():
    xdg = os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
    return os.path.join(xdg, "btdm")


class Twitch:
    """Talks to Twitch as the stored account. Never surfaces the token."""

    def __init__(self, data_dir):
        db = os.path.join(data_dir, "state.db")
        if not os.path.exists(db):
            sys.exit(f"no state.db in {data_dir} — pass --data-dir")
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        row = con.execute(
            "SELECT access_token, login, device_id FROM tokens LIMIT 1"
        ).fetchone()
        con.close()
        if not row:
            sys.exit("no linked account in state.db — run `btdm login` first")
        self._token, self.login, self._device = row

    def query(self, text, variables=None, op=None):
        body = {"query": text}
        if variables:
            body["variables"] = variables
        if op:
            body["operationName"] = op
        request = urllib.request.Request(
            GQL_URL,
            data=json.dumps(body).encode(),
            headers={
                "Authorization": f"OAuth {self._token}",
                "Client-Id": CLIENT_ID,
                "User-Agent": USER_AGENT,
                "X-Device-Id": self._device,
                "Content-Type": "application/json",
            },
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.loads(response.read())
        if payload.get("errors"):
            sys.exit(f"Twitch rejected {op or 'the query'}: {payload['errors']}")
        return payload["data"]


def audit(snapshot, twitch):
    problems = []

    def check(ok, message):
        if not ok:
            problems.append(message)

    now = when(snapshot["generated_at"])
    inventory = twitch.query(INVENTORY, None, "Inventory")["currentUser"]["inventory"]
    ledger = {g["id"]: when(g["lastAwardedAt"]) for g in inventory["gameEventDrops"]}
    known = {}
    for campaign in inventory["dropCampaignsInProgress"] or []:
        for tier in campaign["timeBasedDrops"] or []:
            state = tier.get("self") or {}
            known[tier["id"]] = (
                state.get("currentMinutesWatched") or 0,
                bool(state.get("isClaimed")),
            )

    tiers_seen = 0
    for c in snapshot["eligible"]:
        name = c["name"].strip()
        data = twitch.query(
            DETAIL, {"dropID": c["id"], "channelLogin": twitch.login}, "DropCampaignDetails"
        )
        remote = (data.get("user") or {}).get("dropCampaign")
        if not remote:
            problems.append(f"[{name}] offered as mineable, but Twitch has no such campaign")
            continue

        check(remote["status"] == "ACTIVE",
              f"[{name}] offered as mineable while Twitch calls it {remote['status']}")
        check(when(remote["endAt"]) == when(c["ends_at"]),
              f"[{name}] ends {c['ends_at']}, Twitch says {remote['endAt']}")
        check(when(remote["startAt"]) <= now < when(remote["endAt"]),
              f"[{name}] offered as mineable outside its own window")
        check(remote["game"]["name"] == c["game"],
              f"[{name}] game {c['game']!r}, Twitch says {remote['game']['name']!r}")

        # Twitch's persisted query omits displayName where the raw fallback
        # returns it, so the miner shows the login for some channels. Either
        # spelling identifies the same channel.
        channels = (remote.get("allow") or {}).get("channels") or []
        accept = {ch["name"].lower() for ch in channels}
        accept |= {(ch.get("displayName") or "").lower().strip() for ch in channels}
        shown = [x.lower().strip() for x in c["allowed_channels"]]
        check(len(shown) == len(channels),
              f"[{name}] allow-list holds {len(shown)} channels, Twitch has {len(channels)}")
        stranger = [x for x in shown if x not in accept]
        check(not stranger, f"[{name}] allow-list holds channels Twitch does not: {stranger[:5]}")

        by_id = {t["id"]: t for t in remote["timeBasedDrops"]}
        check(len(by_id) == len(c["drops"]),
              f"[{name}] shows {len(c['drops'])} tiers, Twitch has {len(by_id)}")

        for shown_tier in c["drops"]:
            tiers_seen += 1
            label = f"[{name}/{shown_tier['name'].strip()}]"
            tier = by_id.get(shown_tier["id"])
            if not tier:
                problems.append(f"{label} is not a tier Twitch lists")
                continue

            check(shown_tier["required_minutes"] == tier["requiredMinutesWatched"],
                  f"{label} needs {shown_tier['required_minutes']} min, "
                  f"Twitch says {tier['requiredMinutesWatched']}")

            state = tier.get("self") or {}
            opens = when(tier["startAt"]) if tier.get("startAt") else when(remote["startAt"])
            closes = when(tier["endAt"]) if tier.get("endAt") else when(remote["endAt"])
            benefits = [e["benefit"]["id"] for e in tier["benefitEdges"]]
            awarded = bool(benefits) and all(
                b in ledger and opens <= ledger[b] < closes for b in benefits
            )
            watched, claimed_here = known.get(tier["id"], (0, False))
            truth = bool(state.get("isClaimed")) or claimed_here or awarded
            check(shown_tier["is_claimed"] == truth,
                  f"{label} claimed={shown_tier['is_claimed']} but Twitch says {truth} "
                  f"(campaign={state.get('isClaimed')} inventory={claimed_here} ledger={awarded})")

            # A few minutes behind Twitch is the sync interval, not a defect.
            # Ahead of it, or past the requirement without being claimed, is not.
            check(shown_tier["current_minutes"] >= watched - 5,
                  f"{label} shows {shown_tier['current_minutes']} min, "
                  f"Twitch's inventory says {watched}")
            check(shown_tier["current_minutes"] <= shown_tier["required_minutes"]
                  or shown_tier["is_claimed"],
                  f"{label} shows {shown_tier['current_minutes']}/"
                  f"{shown_tier['required_minutes']} and is not claimed")
            check(0 <= shown_tier["percent"] <= 100, f"{label} percent is {shown_tier['percent']}")

        unclaimed = [t for t in c["drops"] if not t["is_claimed"]]
        check(not (unclaimed and c["remaining_minutes"] == 0),
              f"[{name}] claims no work left with {len(unclaimed)} tiers unclaimed")
        left = (when(c["ends_at"]) - now).total_seconds() / 60
        check(abs((left - c["remaining_minutes"]) - c["slack_minutes"]) <= 2,
              f"[{name}] slack is {c['slack_minutes']} but the deadline leaves "
              f"{left:.0f} - {c['remaining_minutes']}")
        check(c["at_risk"] == (c["slack_minutes"] < AT_RISK_SLACK_MINUTES),
              f"[{name}] at_risk={c['at_risk']} with {c['slack_minutes']} minutes of slack")

    # The summary line must equal the rows it summarises.
    totals = snapshot["totals"]
    rows = snapshot["eligible"]
    check(totals["campaigns_eligible"] == len(rows),
          f"totals claim {totals['campaigns_eligible']} campaigns, {len(rows)} rows shown")
    check(totals["minutes_remaining"] == sum(c["remaining_minutes"] for c in rows),
          f"totals claim {totals['minutes_remaining']} minutes, rows add to "
          f"{sum(c['remaining_minutes'] for c in rows)}")
    outstanding = sum(1 for c in rows for d in c["drops"] if not d["is_claimed"])
    check(totals["drops_remaining"] == outstanding,
          f"totals claim {totals['drops_remaining']} drops left, rows hold {outstanding}")
    check(totals["campaigns_at_risk"] == sum(1 for c in rows if c["at_risk"]),
          "totals disagree with the rows about how many campaigns are at risk")

    return problems, tiers_seen


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("url", nargs="?", default="http://127.0.0.1:8080")
    parser.add_argument("--data-dir", default=default_data_dir())
    parser.add_argument("--token", default="", help="dashboard bearer token, if bound non-loopback")
    args = parser.parse_args()

    url = args.url.rstrip("/") + "/api/snapshots"
    request = urllib.request.Request(url)
    if args.token:
        request.add_header("Authorization", f"Bearer {args.token}")
    try:
        snapshots = json.load(urllib.request.urlopen(request, timeout=30))
    except OSError as e:
        sys.exit(f"could not reach the dashboard at {args.url}: {e}")
    if not snapshots:
        sys.exit("the miner has not reported a snapshot yet")

    twitch = Twitch(args.data_dir)
    total = 0
    for snapshot in snapshots:
        problems, tiers = audit(snapshot, twitch)
        total += len(problems)
        print(f"{snapshot['account']}: audited {len(snapshot['eligible'])} campaigns, {tiers} tiers")
        for problem in problems:
            print("  -", problem)
    if total:
        print(f"\n{total} disagreements with Twitch")
        return 1
    print("\nno disagreements with Twitch")
    return 0


if __name__ == "__main__":
    sys.exit(main())
