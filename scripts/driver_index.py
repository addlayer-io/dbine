#!/usr/bin/env python3
"""The published driver index (`index-<target>.json` in the `drivers`
release): reading, merging, signing and resolving it.

Drivers version apart from the app (docs/drivers-bajo-demanda.md). Each
target's index lists every published host:

    {"target": "<triple>", "schema": 2, "seq": <unix time, always growing>,
     "drivers": {"<pkg>": {"<version>+p<protocol>.e<epoch>": {
         "file", "size", "sha256", "own_hash", "shared_hash", "manifest",
         "min_app": "0.1.9", "yanked": "reason" | null, "published_at": "…Z"}}}}

A missing `min_app` means any app; a missing `yanked`, not yanked. The app
verifies the index with the updater's minisign key (`index-<target>.json.sig`,
written by `tauri signer sign`) and, among the entries with its catalog's
protocol and epoch, takes the newest one it may run (resolve()).

Commands:

  driver_index.py publish <target> <build-dir> [--dry-run]
      Merge the hosts a build made (<build-dir>/new-files.txt and its
      index-<target>.json) into the published index, sign it and upload it
      with its .sig. The .gz files must be uploaded already. Re-reads the
      published index afterwards and merges again if another run overwrote it.
  driver_index.py sign <index.json>       sign a file and check the signature
  driver_index.py verify <index.json>     check <index.json>.sig
  driver_index.py wire-hash [<rev>]       hash of the app <-> host code
  driver_index.py min-app [--app-version X] [--keep 5]

Signing runs DBINE_INDEX_SIGNER (default `npx --yes @tauri-apps/cli@^2 signer
sign`) with the key in TAURI_SIGNING_PRIVATE_KEY (and its password in
TAURI_SIGNING_PRIVATE_KEY_PASSWORD), which it never prints. The signature is
checked against the updater pubkey in src-tauri/tauri.conf.json (--conf).
"""

import argparse
import datetime
import hashlib
import importlib.util
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RELEASE = "drivers"
DEFAULT_CONF = ROOT / "src-tauri" / "tauri.conf.json"
DEFAULT_SIGNER = "npx --yes @tauri-apps/cli@^2 signer sign"
# The code both sides of the app <-> host pipe are built from: a host built
# from this code runs under an app built from code with the same hash.
WIRE_DIRS = ("crates/dbine-plugin/src", "crates/dbine-driver/src")
WIRE_EXCLUDED = {"tests", "testdata"}
APP_TAG = re.compile(r"^v(\d+\.\d+\.\d+)$")
DRIVER_ID = re.compile(r"^(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)\+p(\d+)\.e(\d+)$")

_spec = importlib.util.spec_from_file_location("make_latest_json", ROOT / "scripts" / "make-latest-json.py")
_mlj = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mlj)


class Fail(Exception):
    """A reason to stop, said in one line."""


# --- versions -----------------------------------------------------------------


def version_key(v):
    """Semver ordering key of `X.Y.Z[-pre][+build]` (build ignored)."""
    m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?", v)
    if not m:
        raise Fail(f"versión inválida: {v}")
    core = tuple(int(x) for x in m.group(1, 2, 3))
    if m.group(4) is None:
        return core + (1, ())
    pre = tuple((0, int(p), "") if p.isdigit() else (1, 0, p) for p in m.group(4).split("."))
    return core + (0, pre)


def parse_id(driver_id):
    """`0.1.10+p1.e3` -> ("0.1.10", 1, 3)."""
    m = DRIVER_ID.match(driver_id)
    if not m:
        raise Fail(f"id de driver inválido: {driver_id}")
    return m.group(1), int(m.group(2)), int(m.group(3))


def same_line(entries, protocol, epoch):
    """The ids of `entries` with this protocol and epoch."""
    out = []
    for i in entries:
        m = DRIVER_ID.match(i)
        if m and int(m.group(2)) == protocol and int(m.group(3)) == epoch:
            out.append(i)
    return out


def newest(entries, protocol, epoch, skip_yanked=True):
    """The newest id of `entries` with this protocol and epoch, or None."""
    ids = [i for i in same_line(entries, protocol, epoch) if not (skip_yanked and entries[i].get("yanked"))]
    return max(ids, key=lambda i: version_key(parse_id(i)[0]), default=None)


def resolve(entries, floor_id, app_version):
    """What an app at `app_version` whose catalog carries `floor_id` runs
    (the app's own rule, crates/dbine-plugin/src/index.rs): the newest entry
    with the floor's protocol and epoch that isn't yanked and whose min_app
    the app meets, never older than the floor."""
    floor_version, p, e = parse_id(floor_id)
    floor_key, app_key = version_key(floor_version), version_key(app_version)
    best, best_key = floor_id, floor_key
    for i in same_line(entries, p, e):
        entry = entries[i]
        if entry.get("yanked"):
            continue
        if entry.get("min_app") and version_key(entry["min_app"]) > app_key:
            continue
        k = version_key(parse_id(i)[0])
        if k > best_key:
            best, best_key = i, k
    return best


# --- min_app ------------------------------------------------------------------


def git(*args, cwd=ROOT, input=None):
    r = subprocess.run(["git", *args], cwd=cwd, input=input, capture_output=True, check=True)
    return r.stdout


def _hash_files(files):
    """`files`: (path relative to the repo, bytes). Line endings aside: a
    Windows checkout has CRLF."""
    h = hashlib.sha256()
    for path, data in sorted(files):
        h.update(path.encode())
        h.update(b"\0")
        h.update(data.replace(b"\r\n", b"\n"))
        h.update(b"\0")
    return h.hexdigest()


def _wire_path(path):
    return not (set(path.split("/")) & WIRE_EXCLUDED) and not path.endswith(".md")


def wire_hash(rev=None, root=ROOT):
    """Hash of WIRE_DIRS in the working tree (rev None) or at a git rev."""
    files = []
    if rev is None:
        for d in WIRE_DIRS:
            for p in (root / d).rglob("*"):
                rel = p.relative_to(root).as_posix()
                if p.is_file() and _wire_path(rel):
                    files.append((rel, p.read_bytes()))
        return _hash_files(files)
    listing = git("ls-tree", "-r", "-z", rev, "--", *WIRE_DIRS, cwd=root).split(b"\0")
    blobs = []
    for line in filter(None, listing):
        meta, path = line.split(b"\t", 1)
        mode, kind, sha = meta.split()
        path = path.decode()
        if kind == b"blob" and _wire_path(path):
            blobs.append((path, sha.decode()))
    if not blobs:
        return _hash_files([])
    out = git("cat-file", "--batch", cwd=root, input="".join(f"{s}\n" for _, s in blobs).encode())
    pos = 0
    for path, _ in blobs:
        nl = out.index(b"\n", pos)
        size = int(out[pos:nl].split()[2])
        files.append((path, out[nl + 1 : nl + 1 + size]))
        pos = nl + 1 + size + 1
    return _hash_files(files)


def app_tags(keep, root=ROOT):
    """The last `keep` app release versions (tags `vX.Y.Z`), newest first."""
    tags = [m.group(1) for t in git("tag", "--list", "v*", cwd=root).decode().split() if (m := APP_TAG.match(t))]
    tags.sort(key=version_key, reverse=True)
    return tags[:keep]


def auto_min_app(head_wire, released, app_version=None):
    """The oldest app that can run a host built now.

    `released`: (version, wire_hash) of the last app releases. `app_version`:
    the app being released with these hosts, if any (it runs them by
    definition, so it's the answer when no older release matches). None when
    no app can run them: release the app first."""
    candidates = [v for v, w in released if w == head_wire and v != app_version]
    if app_version:
        candidates.append(app_version)
    return min(candidates, key=version_key, default=None)


def min_app_from_git(keep=5, app_version=None, root=ROOT):
    head = wire_hash(None, root)
    released = [(v, wire_hash(f"v{v}", root)) for v in app_tags(keep, root) if v != app_version]
    return auto_min_app(head, released, app_version)


def raise_min_app(auto, declared):
    """`[package.metadata.dbine] min-app` of a driver can only raise it."""
    if not declared:
        return auto
    version_key(declared)
    return max(auto, declared, key=version_key)


# --- the index ----------------------------------------------------------------


def empty(target):
    return {"target": target, "schema": 2, "seq": 0, "drivers": {}}


def load(path, target=None):
    index = json.loads(Path(path).read_text("utf-8"))
    if target and index.get("target") != target:
        raise Fail(f"{path}: el índice es de {index.get('target')}, no de {target}")
    index.setdefault("drivers", {})
    return index


def dump(index):
    """The bytes that get signed and uploaded."""
    return json.dumps(index, ensure_ascii=False, indent=1).encode("utf-8")


def stamp(index, now=None):
    """Schema 2 and a `seq` above the published one: the app refuses an index
    older than the one it has (a replayed old index)."""
    now = int(time.time() if now is None else now)
    seq = max(now, int(index.get("seq") or 0) + 1)
    out = {"target": index["target"], "schema": 2, "seq": seq}
    out.update({k: v for k, v in index.items() if k not in out})
    return out


def merge(current, local, files):
    """Add to `current` the entries of `local` whose file is in `files` (the
    hosts this build uploaded). Returns how many were added. An id already
    published with another file is an error: a published id never changes."""
    added = 0
    files = set(files)
    for pkg, entries in local["drivers"].items():
        for driver_id, entry in entries.items():
            if entry.get("file") not in files:
                continue
            have = current["drivers"].get(pkg, {}).get(driver_id)
            if have:
                if have.get("sha256") != entry.get("sha256"):
                    raise Fail(f"{pkg} {driver_id} ya está publicado con otro archivo; un id publicado no cambia")
                continue
            current["drivers"].setdefault(pkg, {})[driver_id] = entry
            added += 1
    return added


def drop(index, files):
    """Take out of `index` the entries whose file is in `files`."""
    removed = 0
    files = set(files)
    for entries in index["drivers"].values():
        for driver_id in [i for i, e in entries.items() if e.get("file") in files]:
            del entries[driver_id]
            removed += 1
    return removed


# --- signing ------------------------------------------------------------------


def sign(path, signer=None, conf=DEFAULT_CONF):
    """Sign `path` with `tauri signer sign` (writes <path>.sig) and check the
    signature against the updater pubkey."""
    path = Path(path)
    if not (os.environ.get("TAURI_SIGNING_PRIVATE_KEY") or os.environ.get("TAURI_SIGNING_PRIVATE_KEY_PATH")):
        raise Fail("falta TAURI_SIGNING_PRIVATE_KEY: el índice no se puede firmar")
    cmd = shlex.split(signer or os.environ.get("DBINE_INDEX_SIGNER") or DEFAULT_SIGNER)
    # On Windows npx is npx.cmd, which CreateProcess only finds by its full name.
    cmd[0] = shutil.which(cmd[0]) or cmd[0]
    sig = path.with_name(path.name + ".sig")
    sig.unlink(missing_ok=True)
    # The output (the signature and public key) isn't needed: not shown.
    try:
        r = subprocess.run([*cmd, str(path)], cwd=ROOT, capture_output=True, text=True, stdin=subprocess.DEVNULL)
    except OSError as e:
        raise Fail(f"no se pudo firmar {path.name}: {e}")
    if r.returncode or not sig.is_file():
        # The signer's last error line (it never prints the key).
        last = (r.stderr or "").strip().splitlines()[-1:] or [""]
        raise Fail(f"no se pudo firmar {path.name} (código {r.returncode}): {last[0][:200]}")
    verify(path, conf)
    return sig


def verify(path, conf=DEFAULT_CONF):
    """Raise unless <path>.sig is a good signature of `path` by the updater key."""
    path = Path(path)
    sig_file = path.with_name(path.name + ".sig")
    if not sig_file.is_file():
        raise Fail(f"falta {sig_file.name}")
    try:
        key_id, public = _mlj.parse_pubkey(_mlj.read_pubkey(conf))
        sig = _mlj.parse_signature(sig_file.read_text("utf-8"))
    except _mlj.Fail as e:
        raise Fail(str(e))
    if sig["key_id"] != key_id:
        raise Fail(f"{path.name}: firmado con otra clave que la de {Path(conf).name}")
    data = path.read_bytes()
    message = hashlib.blake2b(data, digest_size=64).digest() if sig["prehashed"] else data
    if not _mlj.ed25519_verify(public, message, sig["sig"]):
        raise Fail(f"{path.name}: la firma no corresponde al archivo")
    if not _mlj.ed25519_verify(public, sig["sig"] + sig["trusted"].encode("utf-8"), sig["global"]):
        raise Fail(f"{path.name}: la firma del comentario de confianza no es válida")


# --- the published copy -------------------------------------------------------


def gh(*args):
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def release_assets():
    """name -> {id, created_at} of the `drivers` release's assets."""
    repo = gh("repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner").strip()
    rid = gh("api", f"repos/{repo}/releases/tags/{RELEASE}", "--jq", ".id").strip()
    lines = gh("api", "--paginate", f"repos/{repo}/releases/{rid}/assets?per_page=100", "--jq", ".[] | {id, name, created_at}")
    assets = [json.loads(l) for l in lines.splitlines() if l.strip()]
    return repo, {a["name"]: a for a in assets}


def download(target, directory):
    """The published index of `target` in `directory`, or None if there is
    none yet."""
    name = f"index-{target}.json"
    _, assets = release_assets()
    if name not in assets:
        return None
    gh("release", "download", RELEASE, "-p", name, "-D", str(directory), "--clobber")
    return load(Path(directory) / name, target)


def update_published(target, change, work, dry_run=False, signer=None, conf=DEFAULT_CONF, attempts=4, settle=45, must_exist=False):
    """Apply `change(index) -> number of changes` to the published index of
    `target`, sign and upload it. Two runs can write the index at once (an
    app release and a driver release): after uploading, wait and read it
    again, and apply the change again if it got lost. Returns the index
    written (or that would be, with dry_run)."""
    work = Path(work)
    for attempt in range(attempts):
        fresh = work / f"published-{attempt}"
        fresh.mkdir(parents=True, exist_ok=True)
        index = download(target, fresh)
        if index is None:
            if must_exist:
                raise Fail(f"no hay index-{target}.json publicado")
            index = empty(target)
        n = change(index)
        if not n:
            print(f"index-{target}.json: " + ("publicado y verificado" if attempt else "sin cambios"), flush=True)
            return index
        index = stamp(index)
        out = work / "out" / f"index-{target}.json"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(dump(index))
        if dry_run:
            print(f"index-{target}.json: {n} cambios (simulado, no se sube)", flush=True)
            return index
        sign(out, signer, conf)
        gh("release", "upload", RELEASE, str(out), str(out) + ".sig", "--clobber")
        print(f"index-{target}.json: {n} cambios subidos (seq {index['seq']})", flush=True)
        time.sleep(settle)
    raise Fail(f"index-{target}.json: otra ejecución lo sigue pisando; reintentá")


# --- commands -----------------------------------------------------------------


def cmd_publish(args):
    build = Path(args.build_dir)
    local = load(build / f"index-{args.target}.json", args.target)
    files = [l.strip() for l in (build / "new-files.txt").read_text("utf-8").splitlines() if l.strip()]
    if not files:
        print("No hay drivers nuevos: el índice no cambia.")
        return
    with tempfile.TemporaryDirectory() as tmp:
        update_published(args.target, lambda index: merge(index, local, files), tmp, args.dry_run, conf=args.conf, settle=args.settle)


def main(argv=None):
    ap = argparse.ArgumentParser(description="El índice de drivers publicado.")
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("publish")
    p.add_argument("target")
    p.add_argument("build_dir")
    p.add_argument("--dry-run", action="store_true")
    p.add_argument("--conf", default=str(DEFAULT_CONF))
    p.add_argument("--settle", type=int, default=45, help="segundos antes de releer el índice subido")
    p = sub.add_parser("sign")
    p.add_argument("file")
    p.add_argument("--conf", default=str(DEFAULT_CONF))
    p = sub.add_parser("verify")
    p.add_argument("file")
    p.add_argument("--conf", default=str(DEFAULT_CONF))
    p = sub.add_parser("wire-hash")
    p.add_argument("rev", nargs="?")
    p = sub.add_parser("min-app")
    p.add_argument("--app-version")
    p.add_argument("--keep", type=int, default=5)
    args = ap.parse_args(argv)
    try:
        if args.cmd == "publish":
            cmd_publish(args)
        elif args.cmd == "sign":
            sign(args.file, conf=args.conf)
            print(f"{args.file}.sig: firma verificada")
        elif args.cmd == "verify":
            verify(args.file, args.conf)
            print(f"{args.file}: firma válida")
        elif args.cmd == "wire-hash":
            print(wire_hash(args.rev))
        elif args.cmd == "min-app":
            v = min_app_from_git(args.keep, args.app_version)
            if not v:
                raise Fail("ninguna de las últimas versiones de la app puede correr un driver compilado ahora: publicá primero la app")
            print(v)
    except Fail as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
