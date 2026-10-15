#!/usr/bin/env python3
"""The release app is compiled as if every driver were in it.

The release build (`--no-default-features --features plugins`) leaves out
the driver crates the app downloads, but the drivers turn on features of
libraries the app shares with them (serde_json's key order, flate2's zlib,
rustls' providers…). The `plugins` feature turns those on again
(src-tauri/Cargo.toml); this checks nothing else differs: every crate the
release build compiles, build tools included, is compiled with the same
features and profile as in the build with every driver, except
dbine-drivers (which drivers it has is the point) and the app crate itself
(its own `all-drivers` / `default` features, which its code doesn't read).

  scripts/check-app-features.py [<target>…]   (default: this machine's)

It reads cargo's unit graph (no compiling; RUSTC_BOOTSTRAP for the
unstable flag, this command only). When it fails, add the features it
lists to the pins in src-tauri/Cargo.toml.
"""

import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXEMPT = {"dbine", "dbine-drivers"}


def units(target, features):
    cmd = ["cargo", "build", "-p", "dbine", "--release", *features, "--target", target, "--unit-graph", "-Z", "unstable-options"]
    out = subprocess.run(cmd, cwd=ROOT, env=dict(os.environ, RUSTC_BOOTSTRAP="1"), check=True, stdout=subprocess.PIPE).stdout
    found = {}
    for u in json.loads(out)["units"]:
        profile = {k: v for k, v in u["profile"].items() if k != "name"}
        key = (u["pkg_id"], u["target"]["name"], tuple(u["target"]["kind"]), u["mode"], u.get("platform"))
        found.setdefault(key, set()).add((tuple(sorted(u["features"])), json.dumps(profile, sort_keys=True)))
    return found


def name(pkg_id):
    # "registry+…#serde_json@1.0.151", "path+file:///…/src-tauri#dbine@0.1.12"
    # or "path+file:///…/crates/dbine-drivers#0.1.12".
    tail = pkg_id.rsplit("#", 1)[-1]
    return tail.split("@")[0] if "@" in tail else pkg_id.rsplit("#", 1)[0].rstrip("/").rsplit("/", 1)[-1]


def main():
    host = re.search(r"^host: (\S+)", subprocess.run(["rustc", "-vV"], check=True, stdout=subprocess.PIPE, text=True).stdout, re.M).group(1)
    failed = False
    for target in sys.argv[1:] or [host]:
        every = units(target, ["--features", "plugins,tauri/custom-protocol"])
        release = units(target, ["--no-default-features", "--features", "plugins,tauri/custom-protocol"])
        diffs = []
        for key, variants in sorted(release.items(), key=lambda kv: str(kv[0])):
            if name(key[0]) in EXEMPT or variants <= every.get(key, set()):
                continue
            before = every.get(key, set())
            for feats, _ in variants:
                missing = sorted({f for b, _ in before for f in b} - set(feats))
                side = "build tools" if key[4] is None else "app"
                diffs.append(f"  {name(key[0])} ({key[1]}, {key[3]}, {side}): without {missing}" if before else f"  {name(key[0])} ({key[1]}, {key[3]}, {side}): not in the build with every driver")
        if diffs:
            failed = True
            print(f"{target}: the release app compiles these crates differently from the build with every driver:")
            print("\n".join(diffs))
        else:
            print(f"{target}: same crates, features and profile as the build with every driver ({len(release)} units)")
    if failed:
        sys.exit("Add those features to the pins in src-tauri/Cargo.toml (feature `plugins`).")


if __name__ == "__main__":
    main()
