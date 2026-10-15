#!/usr/bin/env python3
"""Render the Flathub files for one release (docs/flathub.md).

    python3 flatpak/render.py --tag v0.1.12 --commit <sha> --out <dir> \
        [--env DBINE_GOOGLE_CLIENT_ID=...]

Writes into <dir>:
- io.addlayer.dbine.yml: the manifest template with the tag and commit
  (and the given build environment variables, e.g. the cloud client IDs);
- io.addlayer.dbine.metainfo.xml: the metainfo with <releases> filled from
  CHANGELOG.md, latest first. The tag's version must have a section there.

cargo-sources.json and node-sources.json come from flatpak-builder-tools
(the workflow runs them); this script doesn't touch them.
"""

import argparse
import html
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
HERE = ROOT / "flatpak"
APP_ID = "io.addlayer.dbine"

VERSION = re.compile(r"^## \[(\d+\.\d+\.\d+)\] - (\d{4}-\d{2}-\d{2})\s*$")
SECTION = re.compile(r"^### (.+?)\s*$")
BULLET = re.compile(r"^- (.*)$")
BOLD = re.compile(r"^\*\*(.+?)\*\*")
# Store listings never name other products (owner's rule); a changelog line
# that does (e.g. connection import from another tool) is left out here.
NOT_IN_STORE = re.compile(
    r"dbeaver|dbgate|datagrip|jetbrains|toad|liquibase|bulkshift|navicat|tableplus|heidisql|beekeeper|azure data studio|ssms|management studio",
    re.I,
)


def plain(md: str) -> str:
    """Markdown inline text as plain text."""
    md = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", md)
    md = md.replace("**", "").replace("`", "")
    return re.sub(r"\s+", " ", md).strip()


def item_text(bullet: str) -> str:
    """The bullet's bold title, or its first sentence."""
    m = BOLD.match(bullet)
    if m:
        title = plain(m.group(1)).rstrip(":.").strip()
        rest = plain(bullet[m.end():]).lstrip(":").strip()
        first = re.split(r"(?<=[.!?])\s", rest, maxsplit=1)[0] if rest else ""
        return f"{title}: {first}" if first else title
    return re.split(r"(?<=[.!?])\s", plain(bullet), maxsplit=1)[0]


def parse_changelog(text: str):
    """[(version, date, [(section, [items])])], in file order (latest first)."""
    releases, cur, sec = [], None, None
    for line in text.splitlines():
        if line.startswith("## "):
            m = VERSION.match(line)
            cur = None
            if m:
                cur = (m.group(1), m.group(2), [])
                releases.append(cur)
            continue
        if cur is None:
            continue
        m = SECTION.match(line)
        if m:
            sec = (m.group(1), [])
            cur[2].append(sec)
            continue
        m = BULLET.match(line)
        if m and sec is not None:
            sec[1].append(m.group(1))
        elif line.startswith("  ") and sec is not None and sec[1]:
            sec[1][-1] += " " + line.strip()
    return releases


def releases_xml(releases, indent="    ") -> str:
    out = []
    for version, date, sections in releases:
        body = []
        for name, bullets in sections:
            items = [item_text(b) for b in bullets if not NOT_IN_STORE.search(b)]
            if not items:
                continue
            body.append(f"{indent}    <p>{html.escape(name, quote=False)}</p>")
            body.append(f"{indent}    <ul>")
            body += [f"{indent}      <li>{html.escape(i, quote=False)}</li>" for i in items]
            body.append(f"{indent}    </ul>")
        out.append(f'{indent}<release version="{version}" date="{date}">')
        out.append(f"{indent}  <url type=\"details\">https://github.com/addlayer-io/dbine/releases/tag/v{version}</url>")
        if body:
            out.append(f"{indent}  <description>")
            out += body
            out.append(f"{indent}  </description>")
        out.append(f"{indent}</release>")
    return "\n".join(out)


def render_metainfo(template: str, releases) -> str:
    block = "<releases>\n" + releases_xml(releases) + "\n  </releases>"
    new, n = re.subn(r"<releases>.*?</releases>", lambda _: block, template, flags=re.S)
    if n != 1:
        sys.exit("render.py: the metainfo template needs exactly one <releases> element")
    return new


def render_manifest(template: str, tag: str, commit: str, env: list[str]) -> str:
    out = template.replace("@TAG@", tag).replace("@COMMIT@", commit)
    lines = []
    for kv in env:
        k, sep, v = kv.partition("=")
        if not sep or not re.fullmatch(r"[A-Z_][A-Z0-9_]*", k):
            sys.exit(f"render.py: bad --env {kv!r} (KEY=value)")
        if v:
            lines.append(f"        {k}: '{v.replace(chr(39), chr(39) * 2)}'")
    marker = "        # @CLOUD_ENV@"
    if marker not in out:
        sys.exit("render.py: the manifest template lost its @CLOUD_ENV@ marker")
    return out.replace(marker + "\n", "".join(l + "\n" for l in lines))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tag", required=True, help="the release tag, e.g. v0.1.12")
    ap.add_argument("--commit", required=True, help="the tag's full commit hash")
    ap.add_argument("--out", required=True, type=pathlib.Path)
    ap.add_argument("--env", action="append", default=[], help="KEY=value for the dbine module's build env (empty values are skipped)")
    ap.add_argument("--changelog", type=pathlib.Path, default=ROOT / "CHANGELOG.md")
    a = ap.parse_args()

    if not re.fullmatch(r"[0-9a-f]{40}", a.commit):
        sys.exit("render.py: --commit must be a full 40-character hash")
    version = a.tag.removeprefix("v")
    releases = parse_changelog(a.changelog.read_text(encoding="utf-8"))
    if not releases or releases[0][0] != version:
        found = releases[0][0] if releases else "none"
        sys.exit(f"render.py: CHANGELOG.md's latest version is {found}, not {version}")

    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / f"{APP_ID}.yml").write_text(
        render_manifest((HERE / f"{APP_ID}.yml").read_text(encoding="utf-8"), a.tag, a.commit, a.env), encoding="utf-8"
    )
    (a.out / f"{APP_ID}.metainfo.xml").write_text(
        render_metainfo((HERE / f"{APP_ID}.metainfo.xml").read_text(encoding="utf-8"), releases), encoding="utf-8"
    )
    print(f"rendered {a.tag} ({a.commit[:12]}) into {a.out}: {len(releases)} releases")


if __name__ == "__main__":
    main()
