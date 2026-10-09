#!/usr/bin/env python3
"""CHANGELOG.md: the user-facing changes of each version.

Every change lands under `## [Sin publicar]`; a release turns that section
into `## [<version>] - <date>`. The release workflow publishes a version's
section as the GitHub release notes and the update dialog's text.

  scripts/changelog.py check                 the file parses, versions go down
  scripts/changelog.py notes <version>       print that version's section body
  scripts/changelog.py recent <version> [--count 5]
                                             that version and the ones before it,
                                             each under "## Versión x.y.z" (the
                                             update dialog keeps those newer than
                                             the installed app)
  scripts/changelog.py release <version> [--date YYYY-MM-DD]
                                             «Sin publicar» becomes <version>
"""

import argparse
import datetime
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FILE = ROOT / "CHANGELOG.md"
UNRELEASED = "Sin publicar"
HEADER = re.compile(r"^## \[(?P<name>[^\]]+)\](?: - (?P<date>\d{4}-\d{2}-\d{2}))?\s*$")


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


def check(text):
    _, secs = sections(text)
    errors = []
    names = [n for n, _, _ in secs]
    if UNRELEASED not in names:
        errors.append(f"falta la sección «{UNRELEASED}»")
    elif names[0] != UNRELEASED:
        errors.append(f"«{UNRELEASED}» tiene que ser la primera sección")
    versions = [n for n in names if n != UNRELEASED]
    for v in versions:
        if not re.fullmatch(r"\d+\.\d+\.\d+", v):
            errors.append(f"versión mal escrita: «{v}»")
    good = [v for v in versions if re.fullmatch(r"\d+\.\d+\.\d+", v)]
    if good != sorted(good, key=version_key, reverse=True):
        errors.append("las versiones no van de la más nueva a la más vieja")
    if len(set(names)) != len(names):
        errors.append("hay secciones repetidas")
    for n, d, _ in secs:
        if n != UNRELEASED and not d:
            errors.append(f"«{n}» no tiene fecha")
    return errors


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
            out.append(f"## Versión {n}\n\n{body}")
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
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("check")
    n = sub.add_parser("notes")
    n.add_argument("version")
    rc = sub.add_parser("recent")
    rc.add_argument("version")
    rc.add_argument("--count", type=int, default=5)
    r = sub.add_parser("release")
    r.add_argument("version")
    r.add_argument("--date", default=datetime.date.today().isoformat())
    args = ap.parse_args()

    text = FILE.read_text("utf-8")
    if args.cmd == "check":
        errors = check(text)
        for e in errors:
            print(f"CHANGELOG.md: {e}", file=sys.stderr)
        sys.exit(1 if errors else 0)
    if args.cmd == "notes":
        body = notes(text, args.version.lstrip("v"))
        if not body:
            sys.exit(f"CHANGELOG.md no tiene la versión {args.version}")
        print(body)
        return
    if args.cmd == "recent":
        body = recent(text, args.version.lstrip("v"), args.count)
        if not body:
            sys.exit(f"CHANGELOG.md no tiene la versión {args.version}")
        print(body)
        return
    if args.cmd == "release":
        version = args.version.lstrip("v")
        if not re.fullmatch(r"\d+\.\d+\.\d+", version):
            sys.exit(f"versión mal escrita: «{args.version}»")
        new = release(text, version, args.date)
        errors = check(new)
        if errors:
            sys.exit("\n".join(errors))
        FILE.write_text(new, "utf-8")
        print(f"«{UNRELEASED}» pasó a {version}")


if __name__ == "__main__":
    main()
