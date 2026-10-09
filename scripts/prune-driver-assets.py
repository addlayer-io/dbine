#!/usr/bin/env python3
"""Drop from the `drivers` release the hosts no supported app downloads.

A GitHub release holds at most 1000 assets, and every app or driver release
adds a host per changed driver and target. A host is kept while it is one of:

(a) the hosts named by the catalogs of the last <keep> app releases (tags
    `v*`, by version): what those apps download first. The names are rebuilt
    from each tag's sources the same way build-driver-hosts.py builds them:
    `dbine-driver-<pkg>-<version>+p<protocol>.e<epoch>-<target>.gz`;
(b) the newest host that isn't yanked of each driver, protocol and epoch, per
    target (the published index);
(c) what each of those app versions switches to on its own: the app's rule
    (driver_index.resolve) with that tag's version and catalog against the
    published index.

Every other `dbine-driver-*.gz` asset is deleted (yanked hosts outside (a)
too) and taken out of its target's index, which is signed again and uploaded
with its .sig (driver_index.update_published). A host that isn't in its index
and went up less than a day ago is left alone: a driver release that hasn't
published its index yet.

Nothing is deleted if the hosts of the newest app release aren't all there
(the naming drifted from build-driver-hosts.py, or the listing failed) or a
target's index is missing.

  scripts/prune-driver-assets.py [--keep 5] [--dry-run]

Needs the tags fetched (`git fetch --tags`), `gh` with write access and, to
sign the indexes, TAURI_SIGNING_PRIVATE_KEY (driver_index.py).
"""

import argparse
import datetime
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import driver_index  # noqa: E402

RELEASE = driver_index.RELEASE
TARGETS = ["aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"]
ASSET = re.compile(r"^dbine-driver-([a-z0-9_]+)-(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?\+p\d+\.e\d+)-(.+)\.gz$")
GRACE = datetime.timedelta(days=1)


def run(cmd, capture=True):
    return subprocess.run(cmd, check=True, text=True, capture_output=capture).stdout


def show(tag, path):
    return run(["git", "show", f"{tag}:{path}"])


def app_tags(keep):
    """The last `keep` app release tags, newest first."""
    tags = [t for t in run(["git", "tag", "--list", "v*"]).split() if re.fullmatch(r"v\d+\.\d+\.\d+", t)]
    tags.sort(key=lambda t: tuple(map(int, t[1:].split("."))), reverse=True)
    return tags[:keep]


def floors_of(tag):
    """pkg -> driver id that the app built from `tag` carries in its catalog."""
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
    return {pkg: f"{versions[pkg]}+p{protocol}.e{epoch}" for pkg in features if pkg not in built_in and pkg in versions}


def file_name(pkg, driver_id, target):
    return f"dbine-driver-{pkg}-{driver_id}-{target}.gz"


def hosts_of(floors):
    """(a): the host files an app with these catalog `floors` downloads, for every target."""
    return {file_name(pkg, i, t) for pkg, i in floors.items() for t in TARGETS}


def keep_set(tags, indexes):
    """The asset names to keep. `tags`: [(app version, floors)], newest
    first; `indexes`: target -> published index."""
    keep = set()
    for _, floors in tags:
        keep |= hosts_of(floors)  # (a)
    for target, index in indexes.items():
        for pkg, entries in index["drivers"].items():
            lines = {driver_index.parse_id(i)[1:] for i in entries if driver_index.DRIVER_ID.match(i)}
            for p, e in lines:  # (b)
                top = driver_index.newest(entries, p, e)
                if top:
                    keep.add(entries[top]["file"])
            for app, floors in tags:  # (c)
                if pkg in floors:
                    chosen = driver_index.resolve(entries, floors[pkg], app)
                    if chosen in entries:
                        keep.add(entries[chosen]["file"])
    return keep


def to_drop(assets, keep, indexes, now):
    """The `dbine-driver-*.gz` assets to delete: not kept, and not a host that
    went up in the last day without being in its index yet."""
    listed = {e["file"] for index in indexes.values() for entries in index["drivers"].values() for e in entries.values()}
    drop = []
    for a in assets:
        m = ASSET.match(a["name"])
        if not m or a["name"] in keep:
            continue
        created = a.get("created_at")
        if a["name"] not in listed and created:
            if now - datetime.datetime.fromisoformat(created.replace("Z", "+00:00")) < GRACE:
                continue
        drop.append(a)
    return drop


def check_safe(newest_tag, newest_hosts, names, indexes):
    """Why nothing may be deleted, or None."""
    missing = sorted(newest_hosts - names)
    if not newest_hosts or missing:
        return f"{newest_tag}: faltan {len(missing)} de {len(newest_hosts)} archivos esperados en el release (p. ej. {missing[:3]}); no se borra nada"
    absent = [t for t in TARGETS if t not in indexes]
    if absent:
        return f"falta el índice de {', '.join(absent)}; no se borra nada"
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", type=int, default=5)
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--settle", type=int, default=45, help="segundos antes de releer cada índice subido")
    args = ap.parse_args()

    names_tags = app_tags(args.keep)
    if not names_tags:
        sys.exit("no hay tags de la app (git fetch --tags)")
    tags = [(t[1:], floors_of(t)) for t in names_tags]

    repo, by_name = driver_index.release_assets()
    assets = list(by_name.values())
    with tempfile.TemporaryDirectory() as tmp:
        indexes = {}
        for target in TARGETS:
            try:
                index = driver_index.download(target, Path(tmp) / "read")
            except (subprocess.CalledProcessError, ValueError, driver_index.Fail) as e:
                sys.exit(f"no pude leer index-{target}.json ({e}); no se borra nada")
            if index is not None:
                indexes[target] = index

        # Fail safe: the names rebuilt for the newest release must all be
        # there, and every target's index.
        reason = check_safe(names_tags[0], hosts_of(tags[0][1]), set(by_name), indexes)
        if reason:
            sys.exit(reason)

        keep = keep_set(tags, indexes)
        now = datetime.datetime.now(datetime.timezone.utc)
        drop = to_drop(assets, keep, indexes, now)
        print(f"versiones que se conservan: {', '.join(names_tags)} ({len(keep)} archivos entre catálogos, más nuevos y a los que pasan solas)")
        print(f"{len(assets)} archivos en el release; se borran {len(drop)}")
        for a in drop:
            print("  -", a["name"])
        if args.dry_run or not drop:
            return

        # The index first: an app that reads it never picks a host about to go.
        gone = {}
        for a in drop:
            gone.setdefault(ASSET.match(a["name"]).group(3), set()).add(a["name"])
        for target, files in sorted(gone.items()):
            if target in indexes:
                driver_index.update_published(target, lambda index, f=files: driver_index.drop(index, f), Path(tmp) / target, settle=args.settle, must_exist=True)
        for a in drop:
            run(["gh", "api", "-X", "DELETE", f"repos/{repo}/releases/assets/{a['id']}"])


if __name__ == "__main__":
    try:
        main()
    except driver_index.Fail as e:
        sys.exit(f"error: {e}")
