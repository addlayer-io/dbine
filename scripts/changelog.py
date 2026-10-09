#!/usr/bin/env python3
"""CHANGELOG.md: the user-facing changes of each version.

`CHANGELOG.md` is English and the source. Every change lands under
`## [Unreleased]`; a release turns that section into
`## [<version>] - <date>`. The translations, `changelog/CHANGELOG.<lang>.md`
(es, pt, fr, it), have the same released versions and no Unreleased section;
the translator agent writes each version's section after the release.

The release workflow publishes a version's English section as the GitHub
release notes and the update dialog's text; the app reads the translation at
the release's tag when its UI isn't in English.

  scripts/changelog.py check                 every file parses, versions go
                                             down, translations match English
  scripts/changelog.py notes <version> [--lang xx]
                                             print that version's section body
  scripts/changelog.py recent <version> [--count 5] [--lang xx]
                                             that version and the ones before it,
                                             each under "## Version x.y.z" (the
                                             update dialog keeps those newer than
                                             the installed app)
  scripts/changelog.py release <version> [--date YYYY-MM-DD]
                                             English «Unreleased» becomes <version>
"""

import argparse
import datetime
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LANGS = ("es", "pt", "fr", "it")
UNRELEASED = "Unreleased"
HEADER = re.compile(r"^## \[(?P<name>[^\]]+)\](?: - (?P<date>\d{4}-\d{2}-\d{2}))?\s*$")
VERSION = re.compile(r"\d+\.\d+\.\d+")


def path_of(lang, root=ROOT):
    if lang == "en":
        return root / "CHANGELOG.md"
    return root / "changelog" / f"CHANGELOG.{lang}.md"


def sections(text):
    """[(name, date, body)] in file order, and the text before the first one."""
    out, head, cur = [], [], None
    for line in text.splitlines(keepends=True):
        m = HEADER.match(line.rstrip("\n"))
        if m:
            cur = [m["name"], m["date"], []]
            out.append(cur)
        elif cur is None:
            head.append(line)
        else:
            cur[2].append(line)
    return "".join(head), [(n, d, "".join(b).strip("\n")) for n, d, b in out]


def version_key(v):
    return tuple(int(x) for x in v.split("."))


def released(text):
    """The released versions, in file order."""
    return [n for n, _, _ in sections(text)[1] if n != UNRELEASED]


def check_versions(secs):
    errors = []
    names = [n for n, _, _ in secs]
    versions = [n for n in names if n != UNRELEASED]
    for v in versions:
        if not VERSION.fullmatch(v):
            errors.append(f"versión mal escrita: «{v}»")
    good = [v for v in versions if VERSION.fullmatch(v)]
    if good != sorted(good, key=version_key, reverse=True):
        errors.append("las versiones no van de la más nueva a la más vieja")
    if len(set(names)) != len(names):
        errors.append("hay secciones repetidas")
    for n, d, _ in secs:
        if n != UNRELEASED and not d:
            errors.append(f"«{n}» no tiene fecha")
    return errors


def check(text):
    """The English file."""
    _, secs = sections(text)
    errors = []
    names = [n for n, _, _ in secs]
    if UNRELEASED not in names:
        errors.append(f"falta la sección «{UNRELEASED}»")
    elif names[0] != UNRELEASED:
        errors.append(f"«{UNRELEASED}» tiene que ser la primera sección")
    return errors + check_versions(secs)


def check_translation(text, english):
    """A translation against the English file's text."""
    _, secs = sections(text)
    errors = []
    if any(n == UNRELEASED for n, _, _ in secs):
        errors.append(f"no puede tener sección «{UNRELEASED}» (se traduce al publicar)")
    errors += check_versions(secs)
    mine, source = released(text), released(english)
    missing = [v for v in source if v not in mine]
    extra = [v for v in mine if v not in source]
    if missing:
        errors.append(
            f"le faltan versiones que están en CHANGELOG.md: {', '.join(missing)}"
            " (correr el agente translator para traducirlas)"
        )
    if extra:
        errors.append(f"tiene versiones que no están en CHANGELOG.md: {', '.join(extra)}")
    dates = {n: d for n, d, _ in sections(english)[1]}
    for n, d, _ in secs:
        if n in dates and d and dates[n] and d != dates[n]:
            errors.append(f"«{n}» tiene fecha {d} y en CHANGELOG.md {dates[n]}")
    return errors


def check_all(root=ROOT):
    """{file name: [errors]} for every file with errors."""
    out = {}
    en_path = path_of("en", root)
    english = en_path.read_text("utf-8")
    if errs := check(english):
        out[en_path.name] = errs
    for lang in LANGS:
        p = path_of(lang, root)
        name = f"changelog/{p.name}"
        if not p.exists():
            out[name] = ["no existe (correr el agente translator)"]
            continue
        if errs := check_translation(p.read_text("utf-8"), english):
            out[name] = errs
    return out


def notes(text, version):
    for n, _, body in sections(text)[1]:
        if n == version:
            return body
    return None


def recent(text, version, count):
    """`version`'s section and the `count - 1` released before it."""
    out, started = [], False
    for n, _, body in sections(text)[1]:
        if n == version:
            started = True
        if started and n != UNRELEASED and body.strip():
            out.append(f"## Version {n}\n\n{body}")
        if len(out) == count:
            break
    return "\n\n".join(out) if started else None


def release(text, version, date):
    head, secs = sections(text)
    if any(n == version for n, _, _ in secs):
        sys.exit(f"la versión {version} ya está en el changelog")
    body = next((b for n, _, b in secs if n == UNRELEASED), None)
    if not body or not body.strip():
        sys.exit(f"«{UNRELEASED}» está vacía: no hay cambios para publicar en {version}")
    out = [head.rstrip("\n") + "\n\n", f"## [{UNRELEASED}]\n\n", f"## [{version}] - {date}\n\n{body}\n"]
    for n, d, b in secs:
        if n == UNRELEASED:
            continue
        out.append(f"\n## [{n}]" + (f" - {d}" if d else "") + "\n\n" + (b + "\n" if b else ""))
    return "".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("check")
    n = sub.add_parser("notes")
    n.add_argument("version")
    n.add_argument("--lang", default="en", choices=("en",) + LANGS)
    rc = sub.add_parser("recent")
    rc.add_argument("version")
    rc.add_argument("--count", type=int, default=5)
    rc.add_argument("--lang", default="en", choices=("en",) + LANGS)
    r = sub.add_parser("release")
    r.add_argument("version")
    r.add_argument("--date", default=datetime.date.today().isoformat())
    args = ap.parse_args()
    root = args.root

    if args.cmd == "check":
        bad = check_all(root)
        for name, errs in bad.items():
            for e in errs:
                print(f"{name}: {e}", file=sys.stderr)
        sys.exit(1 if bad else 0)
    if args.cmd in ("notes", "recent"):
        p = path_of(args.lang, root)
        if not p.exists():
            sys.exit(f"no existe {p.relative_to(root)}")
        text = p.read_text("utf-8")
        version = args.version.lstrip("v")
        body = notes(text, version) if args.cmd == "notes" else recent(text, version, args.count)
        if not body:
            sys.exit(f"{p.relative_to(root)} no tiene la versión {args.version}")
        print(body)
        return
    if args.cmd == "release":
        version = args.version.lstrip("v")
        if not VERSION.fullmatch(version):
            sys.exit(f"versión mal escrita: «{args.version}»")
        p = path_of("en", root)
        new = release(p.read_text("utf-8"), version, args.date)
        errors = check(new)
        if errors:
            sys.exit("\n".join(errors))
        p.write_text(new, "utf-8")
        print(
            f"«{UNRELEASED}» pasó a {version} en CHANGELOG.md. Falta traducir esa sección"
            " a changelog/ (agente translator) y que pase `scripts/changelog.py check`."
        )


if __name__ == "__main__":
    main()
