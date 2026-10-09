#!/usr/bin/env python3
"""Drop from the `drivers` release the hosts no recent app downloads.

A GitHub release holds at most 1000 assets, and every app release adds a
host per changed driver and target. Each app build carries a catalog that
names the exact host files it downloads (scripts/build-driver-hosts.py), so a
host is still needed only while an app version that names it is supported.

This keeps the hosts named by the last <keep> app releases (tags `v*`, by
version) and deletes every other `dbine-driver-*.gz` asset, then removes
those versions from the published `index-<target>.json` files. The names are
rebuilt from each tag's sources the same way build-driver-hosts.py builds
them: `dbine-driver-<pkg>-<version>+p<protocol>.e<epoch>-<target>.gz`.

  scripts/prune-driver-assets.py [--keep 5] [--dry-run]

Needs the tags fetched (`git fetch --tags`) and `gh` with write access.
"""

import argparse
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

RELEASE = "drivers"
TARGETS = ["aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"]
ASSET = re.compile(r"^dbine-driver-([a-z0-9_]+)-(\d+\.\d+\.\d+\+p\d+\.e\d+)-(.+)\.gz$")


def run(cmd, capture=True):
    return subprocess.run(cmd, check=True, text=True, capture_output=capture).stdout


def show(tag, path):
    return run(["git", "show", f"{tag}:{path}"])


def app_tags(keep):
    """The last `keep` app release tags, newest first."""
    tags = [t for t in run(["git", "tag", "--list", "v*"]).split() if re.fullmatch(r"v\d+\.\d+\.\d+", t)]
    tags.sort(key=lambda t: tuple(map(int, t[1:].split("."))), reverse=True)
    return tags[:keep]


def hosts_of(tag):
    """The host files the app built from `tag` downloads, for every target."""
    protocol = re.search(r"pub const PROTOCOL: u32 = (\d+);", show(tag, "crates/dbine-plugin/src/proto.rs")).group(1)
    host_toml = show(tag, "crates/dbine-plugin-host/Cargo.toml")
    epoch = re.search(r"^drivers-epoch = (\d+)", host_toml, re.M).group(1)
    features = re.findall(r'^([a-z0-9_]+) = \["dbine-drivers/', host_toml, re.M)
    lib = show(tag, "crates/dbine-drivers/src/lib.rs")
    built_in = re.findall(r'"([a-z0-9_]+)"', re.search(r"BUILT_IN: &\[&str\] = &\[([^\]]*)\]", lib).group(1))
    versions = {}
    for path in run(["git", "ls-tree", "-r", "--name-only", tag, "crates/drivers"]).split():
        if path.count("/") == 3 and path.endswith("/Cargo.toml"):
            toml = show(tag, path)
            name = re.search(r'^name = "dbine-driver-([a-z0-9_]+)"', toml, re.M)
            version = re.search(r'^version = "([^"]+)"', toml, re.M)
            if name and version:
                versions[name.group(1)] = version.group(1)
    names = set()
    for pkg in features:
        if pkg in built_in or pkg not in versions:
            continue
        for target in TARGETS:
            names.add(f"dbine-driver-{pkg}-{versions[pkg]}+p{protocol}.e{epoch}-{target}.gz")
    return names


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", type=int, default=5)
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    tags = app_tags(args.keep)
    if not tags:
        sys.exit("no hay tags de la app (git fetch --tags)")
    keep = set()
    for tag in tags:
        keep |= hosts_of(tag)
    print(f"versiones que se conservan: {', '.join(tags)} ({len(keep)} archivos)")

    repo = run(["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"]).strip()
    rid = run(["gh", "api", f"repos/{repo}/releases/tags/{RELEASE}", "--jq", ".id"]).strip()
    assets = json.loads("[" + ",".join(run(["gh", "api", "--paginate", f"repos/{repo}/releases/{rid}/assets?per_page=100", "--jq", ".[] | {id, name}"]).split("\n")[:-1]) + "]")

    # Fail safe: the names rebuilt for the newest release must all be there.
    # If they aren't, the naming drifted from build-driver-hosts.py (or the
    # listing failed) and nothing is deleted.
    names = {a["name"] for a in assets}
    newest = hosts_of(tags[0])
    missing = sorted(newest - names)
    if not newest or missing:
        sys.exit(f"{tags[0]}: faltan {len(missing)} de {len(newest)} archivos esperados en el release (p. ej. {missing[:3]}); no se borra nada")

    drop = [a for a in assets if ASSET.match(a["name"]) and a["name"] not in keep]
    print(f"{len(assets)} archivos en el release; se borran {len(drop)}")
    for a in drop:
        print("  -", a["name"])
    if args.dry_run:
        return

    for a in drop:
        run(["gh", "api", "-X", "DELETE", f"repos/{repo}/releases/assets/{a['id']}"])

    # Take the deleted versions out of each target's index.
    gone = {}
    for a in drop:
        pkg, version, target = ASSET.match(a["name"]).groups()
        gone.setdefault(target, set()).add((pkg, version))
    with tempfile.TemporaryDirectory() as tmp:
        run(["gh", "release", "download", RELEASE, "-p", "index-*.json", "-D", tmp, "--clobber"])
        changed = []
        for f in sorted(Path(tmp).glob("index-*.json")):
            index = json.loads(f.read_text("utf-8"))
            removed = 0
            for pkg, version in gone.get(index["target"], ()):
                if index["drivers"].get(pkg, {}).pop(version, None) is not None:
                    removed += 1
            if removed:
                f.write_text(json.dumps(index, indent=2) + "\n", "utf-8")
                changed.append(str(f))
                print(f"{f.name}: {removed} versiones quitadas")
        if changed:
            run(["gh", "release", "upload", RELEASE, *changed, "--clobber"])


if __name__ == "__main__":
    main()
