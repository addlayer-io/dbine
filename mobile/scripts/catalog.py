#!/usr/bin/env python3
"""The phone app's slice of DBine's backend catalog.

The Monitor's labels, groups and notes, and the messages of the phone's
channel, come from DBine in Spanish (as the engines write them). DBine's
desktop translates them with web/src/locales/<lang>/backend.json, 1.2 MB a
language; the phone only needs the ones from those files, about 100 KB.
Writes src/locales/backend/<lang>.json (not committed: made on every dev
run and build).
"""

import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
WEB = ROOT / "web" / "src" / "locales"
OUT = pathlib.Path(__file__).resolve().parents[1] / "src" / "locales" / "backend"


def wanted(files: list) -> bool:
    return any("monitor" in f or f.endswith("link.rs") or "dbine-link" in f for f in files)


def main() -> int:
    ids = json.loads((WEB / "backend.msgids.json").read_text())
    keep = {k for k, files in ids.items() if wanted(files)}
    OUT.mkdir(parents=True, exist_ok=True)
    for lang in ["en", "pt", "fr", "it"]:
        full = json.loads((WEB / lang / "backend.json").read_text())
        sub = {k: full[k] for k in sorted(keep) if k in full}
        (OUT / f"{lang}.json").write_text(json.dumps(sub, ensure_ascii=False, separators=(",", ":")))
    print(f"phone catalog: {len(keep)} messages", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
