#!/usr/bin/env python3
"""Real use of DBine, telemetry or not: the update checks.

Every running DBine reads latest.json to look for an update, at start and
then every 6 hours, also with the usage data turned off (it's the updater,
not telemetry; Flathub builds don't update themselves and don't count). The
app asks for `releases/latest/download/latest.json`, so the checks pile up on
the newest release's latest.json while it's the newest one: its count divided
by the days until the next release is that period's checks per day.

Checks per day are between 1 and about 4 times the installations used that day
(one at start, then one every 6 hours an app stays open). Compared with
Aptabase's daily users (only those with the usage data on) they show the trend
of the whole base. New Mac installations come along (the .dmg, which updates
never download).

    gh api --paginate "repos/OWNER/REPO/releases?per_page=100" | jq -s add > releases.json
    python3 scripts/usage-report.py releases.json

Nothing is sent anywhere: it reads GitHub's public download counts.
"""

import datetime as dt
import json
import sys


def _asset(release: dict, test) -> int:
    return sum(a.get("download_count", 0) for a in release.get("assets", []) if test(a.get("name", "")))


def windows(releases: list, now: dt.datetime) -> list:
    """Each app release while it was the newest: tag, start, days, checks,
    checks per day, new Mac installations per day. Drafts and the `drivers`
    release are left out."""
    apps = [r for r in releases if not r.get("draft") and r.get("tag_name", "").startswith("v") and r.get("published_at")]
    apps.sort(key=lambda r: r["published_at"])
    out = []
    for i, r in enumerate(apps):
        start = dt.datetime.fromisoformat(r["published_at"].replace("Z", "+00:00"))
        end = dt.datetime.fromisoformat(apps[i + 1]["published_at"].replace("Z", "+00:00")) if i + 1 < len(apps) else now
        days = max((end - start).total_seconds() / 86400, 1 / 24)
        checks = _asset(r, lambda n: n == "latest.json")
        dmg = _asset(r, lambda n: n.endswith(".dmg"))
        out.append({
            "tag": r["tag_name"], "start": start, "end": end, "days": days,
            "checks": checks, "checks_per_day": checks / days, "mac_new_per_day": dmg / days,
        })
    return out


def per_day_since(rows: list, since: dt.datetime) -> float:
    """Checks per day from `since` on: each period counts for the part of it
    after `since` (spread evenly over the period)."""
    checks = days = 0.0
    for w in rows:
        overlap = (w["end"] - max(w["start"], since)).total_seconds() / 86400
        if overlap <= 0:
            continue
        checks += w["checks_per_day"] * overlap
        days += overlap
    return checks / days if days else 0.0


def main(path: str) -> None:
    now = dt.datetime.now(dt.timezone.utc)
    rows = windows(json.load(open(path)), now)
    print("release    from        days  checks  checks/day  new Mac/day")
    for w in rows:
        print(f"{w['tag']:10} {w['start']:%Y-%m-%d} {w['days']:5.1f} {w['checks']:7d} {w['checks_per_day']:11.1f} {w['mac_new_per_day']:12.1f}")
    for label, d in (("last 7 days", 7), ("last 30 days", 30)):
        c = per_day_since(rows, now - dt.timedelta(days=d))
        print(f"{label}: {c:.0f} checks/day, about {c / 4:.0f} to {c:.0f} installations used per day")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
