#!/usr/bin/env python3
"""Check the app's translations (web/src/locales and the AI prompts).

The languages come from scripts/i18n/languages.json (the one list). Spanish
is the source.

Structure (every language):
- Every key of every Spanish area file (`es/<area>.json`) exists in the
  language, with the same `{{placeholders}}`, the same HTML markup (`<b>`,
  `<code>`…) and the same `$t(…)` nesting; no language has keys Spanish
  doesn't. Plural keys (`_one`, `_few`, `_many`, `_other`) follow the
  language's own CLDR categories (`plurals` in languages.json): each one it
  needs must be there; Spanish's other categories may be there too.
- Every backend message (`backend.msgids.json`) has a translation in each
  `<lang>/backend.json`, with the same `{0}`… values and markup.
- The lists of languages in the code match languages.json (the UI, Element
  Plus, the AI prompts, the update dialog's changelog translations).

Untranslated (every language but es and en): a value identical to English
is reported as untranslated, in the UI files, backend.json and the AI prompts
(`crates/dbine-ai/prompts/<lang>.json`). Exceptions:
- values without words (numbers, symbols, placeholders only) and technical
  tokens (SQL, JSON, UTF-8, DuckDB, URLs, file names);
- the entries in `scripts/i18n/same_as_en/<lang>.txt`, one per line, for
  terms that are legitimately the same as English in that language:
  `<area>.json:<key>` for UI keys, `backend:<message>` for backend messages,
  `prompts:<key>` for the AI prompts (a line break in a message is
  written `\\n`). A line starting with `#` is a comment.

Report (not an error): Spanish-looking text left in .vue templates.

  python3 scripts/i18n/check.py               everything (the merge and release gate)
  python3 scripts/i18n/check.py --structure   structure only (while translating)
  python3 scripts/i18n/check.py --lang ja     only these languages (repeatable)
  python3 scripts/i18n/check.py --list ja     print ja's untranslated entries

Exit status 1 when something is wrong, so CI can run it.
"""

import argparse
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
HERE = ROOT / "scripts/i18n"
LOC = ROOT / "web/src/locales"
PROMPTS = ROOT / "crates/dbine-ai/prompts"
CONFIG = json.loads((HERE / "languages.json").read_text(encoding="utf-8"))
SOURCE = CONFIG["source"]
LANGUAGES = {l["code"]: l for l in CONFIG["languages"]}
TARGETS = [c for c in LANGUAGES if c != SOURCE]
PLURAL = re.compile(r"^(.*)_(zero|one|two|few|many|other)$")


def languages():
    """The language codes, in languages.json's order (for other scripts)."""
    return list(LANGUAGES)


def flat(d, prefix=""):
    out = {}
    for k, v in d.items():
        key = f"{prefix}{k}"
        if isinstance(v, dict):
            out.update(flat(v, key + "."))
        else:
            out[key] = v
    return out


def load(path):
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}


def holders(s, pat):
    return sorted(set(re.findall(pat, s))) if isinstance(s, str) else []


HTML = r"(?:a|b|i|u|em|strong|code|kbd|pre|br|p|span|small|sub|sup|ul|ol|li|mark)"


def markup(s):
    """What a translation keeps as is: HTML tags (`<b>`, `</code>`) and
    `$t(…)` nestings, as a sorted list. `<ruta>`-style placeholders and
    examples in backticks aren't markup: they get translated."""
    if not isinstance(s, str):
        return []
    tags = re.findall(rf"</?{HTML}(?:\s+[\w-]+=(?:\"[^\"]*\"|'[^']*'))*\s*/?>", s)
    return sorted(tags + re.findall(r"\$t\([^)]*\)", s))


def plural_groups(keys):
    """{base: {category}} for the keys that are i18next plural forms."""
    groups = {}
    for k in keys:
        m = PLURAL.match(k)
        if m and f"{m[1]}_other" in keys:
            groups.setdefault(m[1], set()).add(m[2])
    return groups


def neutral(text):
    """A value that reads the same in every language: no words, or a technical token."""
    if not isinstance(text, str):
        return True
    t = re.sub(r"\{\{[^}]*\}\}|\{\d+\}|\{[a-z_]+\}|<[^>]+>|`[^`]*`", " ", text).strip()
    if not re.search(r"[^\W\d_]", t):
        return True
    tokens = re.split(r"[\s,;:/()·|+]+", t)
    tech = re.compile(
        r"^(?:[A-Z0-9][A-Z0-9_.\-]*|[A-Z][a-z]+[A-Z][\w.]*|[a-z]+[A-Z][\w]*|https?://\S+|[\w.\-]+\.[a-z0-9]{1,5}|[\w.\-]*@[\w.\-]+|[⌘⇧⌥⌃]+\S*|v?\d[\w.\-]*|…|—|-)$"
    )
    return all(not tok or tech.match(tok) for tok in tokens)


def allowlist(lang):
    f = HERE / "same_as_en" / f"{lang}.txt"
    if not f.exists():
        return set()
    out = set()
    for line in f.read_text(encoding="utf-8").splitlines():
        if line.strip() and not line.startswith("#"):
            out.add(line.rstrip().replace("\\n", "\n"))
    return out


def entry(e):
    """An entry as a line of an allowlist (a message's line breaks as `\\n`)."""
    return e.replace("\n", "\\n")


class Report:
    def __init__(self):
        self.problems = 0
        self.untranslated = {}  # lang -> {"ui": [...], "backend": [...], "prompts": [...]}

    def problem(self, msg, n=1):
        self.problems += n
        print(msg)


def summary(keys, n=8):
    return f"{', '.join(keys[:n])}{' …' if len(keys) > n else ''}"


def check_ui(lang, r, untranslated):
    allowed = allowlist(lang)
    plurals = set(LANGUAGES[lang].get("plurals", ["other"]))
    src_plurals = set(LANGUAGES[SOURCE].get("plurals", ["other"]))
    for src in sorted((LOC / SOURCE).glob("*.json")):
        if src.name == "backend.json":
            continue
        es = flat(load(src))
        f = LOC / lang / src.name
        if not f.exists():
            r.problem(f"{lang}/{src.name}: missing file")
            continue
        other = flat(load(f))
        en = flat(load(LOC / "en" / src.name))
        groups = plural_groups(es)

        def needed(k):
            m = PLURAL.match(k)
            if m and m[1] in groups:
                return m[2] in plurals
            return True

        def allowed_key(k):
            if k in es:
                return True
            m = PLURAL.match(k)
            return bool(m and m[1] in groups and (m[2] in plurals or m[2] in src_plurals))

        missing = [k for k in es if needed(k) and other.get(k) in ("", None)]
        # The language's own plural categories that Spanish doesn't have (ru `_few`).
        for base in groups:
            for cat in sorted(plurals - src_plurals):
                k = f"{base}_{cat}"
                if other.get(k) in ("", None):
                    missing.append(k)
        extra = [k for k in other if not allowed_key(k)]

        def ref(k):
            m = PLURAL.match(k)
            return es.get(k) if k in es else es.get(f"{m[1]}_other") if m else None

        present = [k for k in other if allowed_key(k) and ref(k) is not None]
        bad = [k for k in present if holders(ref(k), r"\{\{\s*(\w+)") != holders(other[k], r"\{\{\s*(\w+)")]
        tags = [k for k in present if markup(ref(k)) != markup(other[k])]
        for label, keys in (("missing", missing), ("not in es", extra), ("placeholders differ", bad), ("markup differs", tags)):
            if keys:
                r.problem(f"{lang}/{src.name}: {len(keys)} {label}: {summary(keys)}", len(keys))
        if untranslated:
            for k, v in other.items():
                if k in en and v == en[k] and not neutral(v) and f"{src.name}:{k}" not in allowed:
                    r.untranslated.setdefault(lang, {}).setdefault("ui", []).append(f"{src.name}:{k}")


def check_backend(lang, r, untranslated):
    msgids = load(LOC / "backend.msgids.json")
    cat = load(LOC / lang / "backend.json")
    en = load(LOC / "en" / "backend.json")
    allowed = allowlist(lang)
    missing = [m for m in msgids if not cat.get(m)]
    bad = [m for m in msgids if cat.get(m) and holders(m, r"\{(\d+)\}") != holders(cat[m], r"\{(\d+)\}")]
    tags = [m for m in msgids if cat.get(m) and markup(m) != markup(cat[m])]
    stale = [m for m in cat if m not in msgids]
    if missing:
        r.problem(f"{lang}/backend.json: {len(missing)} messages without translation", len(missing))
    if bad:
        r.problem(f"{lang}/backend.json: {len(bad)} with different {{n}} values, e.g. {bad[0]!r}", len(bad))
    if tags:
        r.problem(f"{lang}/backend.json: {len(tags)} with different markup, e.g. {tags[0]!r}", len(tags))
    if stale:
        print(f"{lang}/backend.json: {len(stale)} messages no longer in the code (can be removed)")
    if untranslated:
        for m in msgids:
            v = cat.get(m)
            # A message that isn't Spanish (an engine term the code matches on)
            # stays as it is in English; only Spanish source text counts.
            if v and v == en.get(m) and v != m and not neutral(v) and f"backend:{m}" not in allowed:
                r.untranslated.setdefault(lang, {}).setdefault("backend", []).append(f"backend:{m}")


def check_prompts(lang, r, untranslated):
    f = PROMPTS / f"{lang}.json"
    if not f.exists():
        r.problem(f"crates/dbine-ai/prompts/{lang}.json: missing file")
        return
    if not untranslated:
        return  # keys, placeholders and tags: the crate's own test
    en, mine, allowed = load(PROMPTS / "en.json"), load(f), allowlist(lang)
    for k, v in mine.items():
        if v == en.get(k) and not neutral(v if isinstance(v, str) else "\n".join(v)) and f"prompts:{k}" not in allowed:
            r.untranslated.setdefault(lang, {}).setdefault("prompts", []).append(f"prompts:{k}")


def codes_in(path, pattern):
    text = (ROOT / path).read_text(encoding="utf-8")
    return re.findall(pattern, text)


def check_wiring(r):
    """The language lists in the code match languages.json."""
    want = set(LANGUAGES)
    found = {
        "web/src/i18n/index.ts (LANGUAGES)": set(codes_in("web/src/i18n/index.ts", r"\{ code: '([a-z]+)'")),
        "web/src/i18n/element.ts (ELEMENT_LOCALES)": set(
            re.findall(r"\b([a-z]{2}): el[A-Z]", (ROOT / "web/src/i18n/element.ts").read_text(encoding="utf-8"))
        ),
        "crates/dbine-ai/src/prompts.rs (LANGS)": set(
            re.findall(r'"([a-z]+)"', re.search(r"LANGS: &\[&str\] = &\[([^\]]*)\]", (ROOT / "crates/dbine-ai/src/prompts.rs").read_text(encoding="utf-8"))[1])
        ),
        "src-tauri/src/commands/updates.rs (TRANSLATED, without en)": set(
            re.findall(r'"([a-z]+)"', re.search(r"const TRANSLATED: \[&str; \d+\] = \[([^\]]*)\]", (ROOT / "src-tauri/src/commands/updates.rs").read_text(encoding="utf-8"))[1])
        )
        | {"en"},
        "web/src/locales/<lang>/": {p.name for p in LOC.iterdir() if p.is_dir()},
    }
    for where, codes in found.items():
        if codes != want:
            parts = []
            if want - codes:
                parts.append(f"missing {', '.join(sorted(want - codes))}")
            if codes - want:
                parts.append(f"not in languages.json: {', '.join(sorted(codes - want))}")
            r.problem(f"{where}: {'; '.join(parts)}")
    mobile = ROOT / "mobile/src/i18n.ts"
    if mobile.exists():
        m = re.search(r"LANGS = \[([^\]]*)\]", mobile.read_text(encoding="utf-8"))
        codes = set(re.findall(r"'([a-z]+)'", m[1])) if m else set()
        if want - codes:
            # The phone app's languages are a separate decision: reported, not an error.
            print(f"note: the phone app (mobile/) doesn't have {', '.join(sorted(want - codes))} yet")


def spanish_leftovers():
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


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--structure", action="store_true", help="skip the untranslated check")
    ap.add_argument("--lang", action="append", choices=TARGETS, help="only these languages")
    ap.add_argument("--list", choices=TARGETS, help="print this language's untranslated entries")
    args = ap.parse_args()
    langs = args.lang or ([args.list] if args.list else TARGETS)
    untranslated = not args.structure

    r = Report()
    for lang in langs:
        check_ui(lang, r, untranslated and lang != "en")
        check_backend(lang, r, untranslated and lang != "en")
        check_prompts(lang, r, untranslated and lang != "en")
    if not args.list:
        check_wiring(r)
        spanish_leftovers()

    if args.list:
        for kind in ("ui", "backend", "prompts"):
            for e in r.untranslated.get(args.list, {}).get(kind, []):
                print(entry(e))
        sys.exit(1 if r.untranslated.get(args.list) else 0)

    total_untranslated = 0
    if r.untranslated:
        print("\nUntranslated (identical to English; legitimate ones go in scripts/i18n/same_as_en/<lang>.txt):")
        for lang in langs:
            u = r.untranslated.get(lang)
            if not u:
                continue
            n = {k: len(u.get(k, [])) for k in ("ui", "backend", "prompts")}
            total_untranslated += sum(n.values())
            print(f"  {lang}: {n['ui']} UI keys, {n['backend']} backend messages, {n['prompts']} AI prompt keys")
    parts = []
    if r.problems:
        parts.append(f"{r.problems} problems")
    if total_untranslated:
        parts.append(f"{total_untranslated} untranslated")
    print(f"\n{'; '.join(parts) if parts else 'OK'}")
    sys.exit(1 if parts else 0)


if __name__ == "__main__":
    main()
