#!/usr/bin/env python3
"""Check the app's translations (web/src/locales).

- Every key of every Spanish area file (`es/<area>.json`, the source) exists
  in en, pt, fr and it, with the same `{{placeholders}}`, and no language has
  keys Spanish doesn't.
- Every backend message (`backend.msgids.json`) has a translation in each
  `<lang>/backend.json`, with the same `{0}`… values.
- Report (not an error): Spanish-looking text left in .vue templates.

Exit status 1 when something is missing, so CI can run it.
"""

import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
LOC = ROOT / "web/src/locales"
LANGS = ["en", "pt", "fr", "it"]


def flat(d, prefix=""):
    out = {}
    for k, v in d.items():
        key = f"{prefix}{k}"
        if isinstance(v, dict):
            out.update(flat(v, key + "."))
        else:
            out[key] = v
    return out


def holders(s, pat):
    return sorted(set(re.findall(pat, s))) if isinstance(s, str) else []


def main():
    problems = 0
    for src in sorted((LOC / "es").glob("*.json")):
        if src.name == "backend.json":
            continue
        es = flat(json.loads(src.read_text(encoding="utf-8")))
        for lang in LANGS:
            f = LOC / lang / src.name
            other = flat(json.loads(f.read_text(encoding="utf-8"))) if f.exists() else {}
            missing = [k for k in es if k not in other or other[k] in ("", None)]
            extra = [k for k in other if k not in es]
            bad = [k for k in es if k in other and holders(es[k], r"\{\{\s*(\w+)") != holders(other[k], r"\{\{\s*(\w+)")]
            for label, keys in (("missing", missing), ("not in es", extra), ("placeholders differ", bad)):
                if keys:
                    problems += len(keys)
                    print(f"{lang}/{src.name}: {len(keys)} {label}: {', '.join(keys[:8])}{' …' if len(keys) > 8 else ''}")

    msgids = json.loads((LOC / "backend.msgids.json").read_text(encoding="utf-8"))
    for lang in LANGS:
        f = LOC / lang / "backend.json"
        cat = json.loads(f.read_text(encoding="utf-8")) if f.exists() else {}
        missing = [m for m in msgids if not cat.get(m)]
        bad = [m for m in msgids if cat.get(m) and holders(m, r"\{(\d+)\}") != holders(cat[m], r"\{(\d+)\}")]
        stale = [m for m in cat if m not in msgids]
        if missing:
            problems += len(missing)
            print(f"{lang}/backend.json: {len(missing)} messages without translation")
        if bad:
            problems += len(bad)
            print(f"{lang}/backend.json: {len(bad)} with different {{n}} values, e.g. {bad[0]!r}")
        if stale:
            print(f"{lang}/backend.json: {len(stale)} messages no longer in the code (can be removed)")

    leftovers = []
    spanish = re.compile(r"[áéíóúñ¿¡]|\b(el|la|los|las|de|del|que|para|con|por|una|sin)\b", re.I)
    for f in sorted((ROOT / "web/src").rglob("*.vue")):
        if "/preview/" in str(f):
            continue
        text = f.read_text(encoding="utf-8")
        tpl = re.search(r"<template>([\s\S]*)</template>", text)
        if not tpl:
            continue
        body = re.sub(r"\{\{[\s\S]*?\}\}", "", tpl.group(1))
        for m in re.finditer(r">([^<>]+)<|\s(?:title|placeholder|label|aria-label|content)=\"([^\"]+)\"", body):
            t = (m.group(1) or m.group(2) or "").strip()
            if len(t) > 2 and spanish.search(t) and not t.startswith(("$t(", "t(")):
                leftovers.append(f"{f.relative_to(ROOT)}: {t[:70]}")
    if leftovers:
        print(f"\n{len(leftovers)} Spanish-looking texts in templates (check they go through $t):")
        for l in leftovers[:40]:
            print("  " + l)

    print(f"\n{'OK' if not problems else f'{problems} problems'}")
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
