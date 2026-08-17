# Auditing the miner against Twitch

    python3 tools/audit-live/audit.py [http://127.0.0.1:8080] [--data-dir DIR] [--token T]

Every other check in this repository asks whether the miner does what the
miner intends. This one asks whether what the miner believes is true, by
putting the same questions to Twitch and comparing the answers.

That distinction is not academic. The bugs it exists to catch are invisible
from the inside, because every layer is faithfully passing along a wrong
answer it was given:

- Twitch's per-campaign `self.isClaimed` reports `false` for rewards the
  account has owned for a week, because it only tracks claims made while the
  campaign sat in the in-progress inventory. Believing it meant offering to
  spend three hours re-earning an R6 Siege skin awarded six days earlier, and
  queueing 24 hours of work across five campaigns that were already finished.
- Feasibility used to be all-or-nothing, so a campaign whose last tier had
  slipped out of reach was dropped without a directory query ever being made —
  and the dashboard then reported "nobody live" for a game with thirty
  drops-enabled streamers on the first page.

Neither showed up in the unit tests, the fixture tests, or the dashboard
render checks, because none of those has an opinion about what Twitch
actually says.

## What it checks

For every campaign the dashboard currently calls mineable:

| | |
|---|---|
| campaign | status, start and end, game, tier count |
| allow-list | membership and size |
| each tier | required minutes, progress, percentage bounds |
| each tier's claim | cross-checked three ways — the campaign's own `self.isClaimed`, the in-progress inventory, and the `gameEventDrops` awarded-benefit ledger |
| each card | slack against its own deadline, at-risk against slack |
| the totals | against the rows they summarise |

It is read-only, and changes nothing about the running miner. Exit status is
non-zero when anything disagrees.

## Security

It reads the OAuth token out of `state.db` so it can query Twitch as the
account, opening the database read-only and never printing the token. That
file is a password file — see the security notes in the main README — so treat
a terminal capture of a run the same way.

## Keeping it honest

A check that cannot fail is worse than no check. To confirm this one still
bites, corrupt a snapshot before auditing it:

```python
import json, urllib.request, sys
sys.path.insert(0, "tools/audit-live")
import audit as A

snap = json.load(urllib.request.urlopen("http://127.0.0.1:8080/api/snapshots"))[0]
snap["eligible"][0]["drops"][0]["required_minutes"] = 999
print(A.audit(snap, A.Twitch(A.default_data_dir()))[0])
```

Flipping a claim flag, inventing an allow-list entry, adding a tier Twitch
does not list, breaking the slack arithmetic and contradicting the totals are
all caught the same way.
