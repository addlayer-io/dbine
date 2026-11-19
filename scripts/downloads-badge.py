#!/usr/bin/env python3
"""The README's downloads badge: how many times DBine itself was downloaded.

It counts every download of the app from the GitHub releases:
- the installers of each platform and format: .dmg, .exe, .msi, .deb,
  .rpm, .AppImage, .flatpak and .apk (Homebrew's cask downloads the .dmg);
- the updates: macOS downloads the .app.tar.gz, while Windows and Linux
  download the installer again.

It leaves out:
- the drivers the app downloads on demand (the `drivers` release): one user
  can download dozens, and that is still one download of the app;
- latest.json, which every running app reads to look for an update;
- the update signatures (.sig) and the build provenance (.jsonl).

    gh api --paginate "repos/OWNER/REPO/releases?per_page=100" | jq -s add > releases.json
    python3 scripts/downloads-badge.py releases.json downloads.json

It writes a shields.io endpoint badge (https://shields.io/badges/endpoint-badge)
that looks like the GitHub downloads badge it replaces.
"""

import json
import sys

# The files of the app itself, by how their names end.
APP_FILES = (".dmg", ".exe", ".msi", ".deb", ".rpm", ".AppImage", ".flatpak", ".apk", ".app.tar.gz")


def counts(tag: str, name: str) -> bool:
    """Whether a download of this release asset is a download of the app."""
    return tag.startswith("v") and name.endswith(APP_FILES)


def total(releases: list) -> int:
    return sum(
        a.get("download_count", 0)
        for r in releases
        if not r.get("draft")
        for a in r.get("assets", [])
        if counts(r.get("tag_name", ""), a.get("name", ""))
    )


def metric(n: int) -> str:
    """The number as shields.io writes download counts: 950, 4.0k, 12k, 1.5M."""
    for limit, prefix in ((10**9, "G"), (10**6, "M"), (10**3, "k")):
        if n >= limit:
            scaled = n / limit
            text = f"{scaled:.1f}" if scaled < 10 else f"{scaled:.0f}"
            # 999.6k rounds to the next prefix, as "1.0M".
            if text == "1000" and prefix != "G":
                return metric(10 ** (len(str(limit)) + 2))
            return text + prefix
    return str(n)


def badge(n: int) -> dict:
    return {
        "schemaVersion": 1,
        "label": "downloads",
        "message": metric(n),
        # The green of the GitHub downloads badge.
        "color": "4b0",
        "cacheSeconds": 3600,
    }


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit("usage: downloads-badge.py releases.json downloads.json")
    with open(sys.argv[1], encoding="utf-8") as f:
        releases = json.load(f)
    n = total(releases)
    with open(sys.argv[2], "w", encoding="utf-8") as f:
        json.dump(badge(n), f)
        f.write("\n")
    print(f"{n} downloads of the app ({metric(n)})")


if __name__ == "__main__":
    main()
