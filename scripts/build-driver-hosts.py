#!/usr/bin/env python3
"""The downloadable drivers of a release, for one platform.

Drivers have their own versions (each crate's `version` in
crates/drivers/<crate>/Cargo.toml) and live in a permanent `drivers` release,
apart from the app's releases. For each driver crate (except the built-in
ones, dbine_drivers::BUILT_IN) this:

- takes its id, `<version>+p<protocol>.e<epoch>`: the protocol the app and
  the driver speak (crates/dbine-plugin/src/proto.rs) and the drivers epoch
  (crates/dbine-plugin-host/Cargo.toml, bumped by hand to republish them all);
- reuses the host already published with that id (the index), or builds it,
  gzips it and adds it to the index;
- fails if the driver's code changed but its version didn't, and warns when
  code it shares with other drivers (dbine-driver, dependencies) changed;
- fails if a new version isn't above every published one with its protocol
  and epoch, or drops an engine id the previous version had (installed apps
  switch to it on their own);
- gives each new host its `min_app`, the oldest app that can run it
  (driver_index.py: the oldest of the last five app releases whose app <->
  host code is this one's; `[package.metadata.dbine] min-app` in the
  driver's Cargo.toml can only raise it).

Writes to <out-dir>:
- index-<target>.json: the updated index (driver_index.py publish merges the
  new hosts into the published one, signs and uploads it);
- the new hosts' .gz files and new-files.txt, the list of them;
- plugins.json: the catalog the app carries (DBINE_PLUGIN_CATALOG);
- summary.md: what was built, reused and warned.

  scripts/build-driver-hosts.py <target> <out-dir> <base-url> [<published index>]
      [--only <pkg>[,<pkg>…]] [--no-catalog] [--app-version X.Y.Z]
      [--shard I/N] [--prebuilt <dir>]

Without --only it's an app release: every driver with a new version gets
built and the app being released (--app-version, by default the workspace
version) is the newest min_app. --only builds just those drivers (a driver
release, .github/workflows/drivers.yml): the other drivers' problems are only
warnings, and if no released app can run what's built it fails (release the
app first). --no-catalog skips plugins.json and its check.

An app release spreads the builds over several CI jobs: --shard I/N builds
only every N-th host to build (from the I-th, counting from 0) into <out-dir>
and writes nothing else (no checks, index or catalog: the full run does
them). The full run then takes --prebuilt <dir>, the shards' .gz files, and
builds only the hosts that aren't there.

Cross-compiling (Windows from macOS): CARGO_BUILD="cargo xwin build".
Hosts to build at once: DBINE_DRIVER_JOBS (default: one per four CPUs, up
to three; extra ones build in target/driver-hosts-<n>).
"""

import argparse
import datetime
import gzip
import hashlib
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import threading
from concurrent.futures import ThreadPoolExecutor, wait
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXCLUDED_DIRS = {"tests", "benches", "examples", "target"}
sys.path.insert(0, str(ROOT / "scripts"))
import driver_index  # noqa: E402


def run(cmd, env=None, capture=False):
    print("+", " ".join(cmd), flush=True)
    r = subprocess.run(cmd, cwd=ROOT, env=env, check=True, stdout=subprocess.PIPE if capture else None)
    return r.stdout.decode("utf-8") if capture else None


def build_jobs(pending):
    """How many hosts to build at once: DBINE_DRIVER_JOBS, or one per four
    CPUs up to three. A host build ends in a long single-threaded link (fat
    LTO, one codegen unit), so a few run side by side well; each extra one
    takes its own target dir (~4 GB) and recompiles the dependencies there
    once. CI runners (2-4 CPUs) get one, as before."""
    jobs = os.environ.get("DBINE_DRIVER_JOBS")
    jobs = int(jobs) if jobs else min(3, (os.cpu_count() or 1) // 4)
    return max(1, min(jobs, pending))


def files_hash(h, directory: Path):
    """Every source file of a crate (tests and docs aside), by path and content."""
    for p in sorted(directory.rglob("*")):
        rel = p.relative_to(directory)
        if not p.is_file() or rel.parts[0] in EXCLUDED_DIRS or p.suffix == ".md":
            continue
        h.update(str(rel).replace(os.sep, "/").encode())
        h.update(b"\0")
        # Line endings aside: a Windows checkout has CRLF (core.autocrlf).
        h.update(p.read_bytes().replace(b"\r\n", b"\n"))
        h.update(b"\0")


def check_new(pkg, version, entries, protocol, epoch, ids):
    """Errors of a version about to be built, against the published `entries`
    of its package: it must be above every published version with this
    protocol and epoch (yanked ones too: a version is never reused), and keep
    every engine id of the previous one (`ids`: the new manifest's)."""
    errors = []
    published = driver_index.same_line(entries, protocol, epoch)
    top = max(published, key=lambda i: driver_index.version_key(driver_index.parse_id(i)[0]), default=None)
    if top and driver_index.version_key(driver_index.parse_id(top)[0]) >= driver_index.version_key(version):
        errors.append(f"- **{pkg}**: la versión {version} no supera la publicada {driver_index.parse_id(top)[0]}. Subí `version` en crates/drivers/{pkg}/Cargo.toml.")
    prev = driver_index.newest(entries, protocol, epoch) or top
    if prev:
        gone = sorted({m["info"]["id"] for m in entries[prev].get("manifest", [])} - set(ids))
        if gone:
            errors.append(f"- **{pkg}** {version}: ya no tiene los motores {', '.join(gone)} que tenía {prev}; las apps instaladas los perderían. Si es a propósito, subí drivers-epoch.")
    return errors


def parse_args(argv=None):
    ap = argparse.ArgumentParser(description="Compila los drivers descargables de una plataforma.")
    ap.add_argument("target")
    ap.add_argument("out")
    ap.add_argument("base_url")
    ap.add_argument("published", nargs="?", default="")
    ap.add_argument("--only", action="append", default=[], help="solo estos drivers (repetible o separados por coma)")
    ap.add_argument("--no-catalog", action="store_true", help="sin plugins.json ni su chequeo")
    ap.add_argument("--app-version", help="la versión de la app que se publica (por defecto, la del workspace)")
    ap.add_argument("--shard", help="I/N: solo uno de cada N drivers a compilar, desde el I (desde 0); solo los .gz")
    ap.add_argument("--prebuilt", help="carpeta con los .gz ya compilados (por --shard): esos no se compilan")
    args = ap.parse_args(argv)
    args.only = {p.strip() for o in args.only for p in o.split(",") if p.strip()}
    if args.shard:
        m = re.fullmatch(r"(\d+)/(\d+)", args.shard)
        if not m or not 0 <= int(m.group(1)) < int(m.group(2)):
            ap.error(f"--shard {args.shard}: tiene que ser I/N con 0 <= I < N")
        args.shard = (int(m.group(1)), int(m.group(2)))
    return args


def main():
    args = parse_args()
    target, out, base_url = args.target, Path(args.out).resolve(), args.base_url
    published = Path(args.published) if args.published else None
    only = args.only
    out.mkdir(parents=True, exist_ok=True)
    exe = ".exe" if "windows" in target else ""
    cargo_build = os.environ.get("CARGO_BUILD", "cargo build").split()

    protocol = int(re.search(r"pub const PROTOCOL: u32 = (\d+);", (ROOT / "crates/dbine-plugin/src/proto.rs").read_text("utf-8")).group(1))
    host_toml = (ROOT / "crates/dbine-plugin-host/Cargo.toml").read_text("utf-8")
    epoch = int(re.search(r"^drivers-epoch = (\d+)", host_toml, re.M).group(1))
    features = re.findall(r'^([a-z0-9_]+) = \["dbine-drivers/', host_toml, re.M)
    lib = (ROOT / "crates/dbine-drivers/src/lib.rs").read_text("utf-8")
    built_in = re.findall(r'"([a-z0-9_]+)"', re.search(r"BUILT_IN: &\[&str\] = &\[([^\]]*)\]", lib).group(1))
    packages = [f for f in features if f not in built_in]
    if only - set(packages):
        sys.exit(f"no son drivers descargables: {', '.join(sorted(only - set(packages)))} (desconocidos o incluidos en la app)")

    meta = json.loads(run(["cargo", "metadata", "--format-version", "1", "--all-features", "--filter-platform", target], capture=True))
    by_id = {p["id"]: p for p in meta["packages"]}
    by_name = {p["name"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    workspace = set(meta["workspace_members"])

    def deps(pid):
        """Normal and build dependencies (not dev) of a package."""
        for d in nodes[pid]["deps"]:
            if any(k["kind"] in (None, "build") for k in d["dep_kinds"]):
                yield d["pkg"]

    def is_driver(pid):
        return pid in workspace and "/crates/drivers/" in by_id[pid]["manifest_path"].replace(os.sep, "/")

    def hashes(pid):
        """(own, shared): what the driver is (its crate, the driver crates it
        links and their direct dependencies' versions), and everything it
        links (for the warning)."""
        own, shared = hashlib.sha256(), hashlib.sha256()
        drivers, stack = [], [pid]
        while stack:
            p = stack.pop()
            if p in drivers:
                continue
            drivers.append(p)
            stack.extend(d for d in deps(p) if is_driver(d))
        for p in sorted(drivers):
            files_hash(own, Path(by_id[p]["manifest_path"]).parent)
            for d in sorted(deps(p)):
                if d not in workspace:
                    own.update(f"{by_id[d]['name']} {by_id[d]['version']}\n".encode())
        closure, stack = set(), [pid]
        while stack:
            p = stack.pop()
            if p in closure:
                continue
            closure.add(p)
            stack.extend(deps(p))
        for p in sorted(closure):
            if p in workspace:
                files_hash(shared, Path(by_id[p]["manifest_path"]).parent)
            else:
                shared.update(f"{by_id[p]['name']} {by_id[p]['version']} {by_id[p].get('source')}\n".encode())
        return own.hexdigest(), shared.hexdigest()

    index = {"target": target, "drivers": {}}
    if published and published.is_file():
        index = json.loads(published.read_text("utf-8"))
        if index.get("target") != target:
            sys.exit(f"el índice publicado es de {index.get('target')}, no de {target}")

    # What each driver says about itself, from this code: the manifest of the
    # versions built now. Built for the target when it runs here (CI), so the
    # drivers compile once.
    host_triple = re.search(r"^host: (\S+)", run(["rustc", "-vV"], capture=True), re.M).group(1)
    native = ["--target", target] if host_triple == target else []
    native_dir = ROOT / "target" / (target if native else "") / "release"
    # With --only, a host with just those drivers says the same about them.
    only_features = ["--no-default-features", "--features", ",".join(sorted(only))] if only else []
    # A shard skips it (and the checks that use it): the full run does them.
    shard = args.shard
    manifest = [] if shard else json.loads(run(["cargo", "run", "--quiet", "-p", "dbine-plugin-host", "--release", *native, *only_features, "--", "--manifest"], capture=True))

    built, reused, errors, warnings, todo, skipped = [], [], [], [], [], []
    for pkg in packages:
        crate = by_name.get(f"dbine-driver-{pkg}")
        if not crate:
            sys.exit(f"no encuentro el crate dbine-driver-{pkg}")
        version = crate["version"]
        driver_id = f"{version}+p{protocol}.e{epoch}"
        # A driver release (--only) builds its drivers; the others' problems
        # are for their own release or the app's, so here they only warn.
        mine = not only or pkg in only
        own, shared = hashes(crate["id"])
        entries = index["drivers"].setdefault(pkg, {})
        entry = entries.get(driver_id)
        if entry:
            if entry["own_hash"] != own:
                msg = f"- **{pkg}**: cambió su código y sigue en la versión {version}. Subí `version` en crates/drivers/{pkg}/Cargo.toml."
                (errors if mine else warnings).append(msg)
            elif entry["shared_hash"] != shared:
                warnings.append(f"- **{pkg}** {version}: cambió código que comparte (dbine-driver, dependencias). Si le afecta, subí su versión o el epoch.")
            reused.append(f"{pkg} {version}")
            continue
        if not mine:
            skipped.append(f"{pkg} {version}")
            continue
        ids = [m["info"]["id"] for m in manifest if m["package"] == pkg]
        if not shard:
            errors += check_new(pkg, version, entries, protocol, epoch, ids)
        declared = ((crate.get("metadata") or {}).get("dbine") or {}).get("min-app")
        try:
            declared and driver_index.version_key(declared)
        except driver_index.Fail:
            errors.append(f"- **{pkg}**: `min-app = \"{declared}\"` en [package.metadata.dbine] no es una versión.")
            declared = None
        todo.append((pkg, version, driver_id, own, shared, declared))

    if shard:
        todo, errors = todo[shard[0]::shard[1]], []

    # The oldest app that can run what gets built now.
    min_app = None
    if todo and not shard:
        app_version = None
        if not only:
            app_version = args.app_version or re.search(r'\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text("utf-8"), re.M | re.S).group(1)
        try:
            if only and not driver_index.app_tags(5):
                sys.exit("no hay tags de la app (git fetch --tags): hacen falta para calcular min_app")
            min_app = driver_index.min_app_from_git(5, app_version)
        except (subprocess.CalledProcessError, OSError) as e:
            if only:
                sys.exit(f"no pude leer las versiones de la app en git ({e}); hacen falta los tags (git fetch --tags)")
            # An app release without git (a container): only this app and newer.
            min_app = app_version
            warnings.append(f"- Sin git para calcular min_app ({e}): los drivers nuevos piden la app {app_version}.")
        if not min_app:
            errors.append("- El código entre la app y los drivers (crates/dbine-plugin, crates/dbine-driver) cambió desde la última versión publicada de la app: ninguna app instalada podría correr estos drivers. Publicá primero la app.")
    todo = [(pkg, version, driver_id, own, shared, driver_index.raise_min_app(min_app, declared) if min_app else None) for pkg, version, driver_id, own, shared, declared in todo]

    # Before building anything: nothing gets published while one of them fails.
    if errors:
        (out / "summary.md").write_text("### Drivers que no se pueden publicar\n\n" + "\n".join(errors) + "\n", "utf-8")
        sys.exit("\n".join(["Drivers que no se pueden publicar:", *errors]))

    # Each host is the same binary with other features, so builds that run at
    # once need their own target dirs (slot 0 keeps the usual one).
    jobs = build_jobs(len(todo))
    base_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    slots = queue.Queue()
    for i in range(jobs):
        slots.put(base_dir if i == 0 else base_dir / f"driver-hosts-{i}")
    failed = threading.Event()

    def build(pkg, driver_id):
        """Build one host and gzip it into `out` (or take it from --prebuilt);
        the file name, or None if skipped after another build failed."""
        file = f"dbine-driver-{pkg}-{driver_id}-{target}.gz"
        if args.prebuilt and (Path(args.prebuilt) / file).is_file():
            print(f"== {pkg} {driver_id}: ya compilado ({args.prebuilt})", flush=True)
            shutil.copyfile(Path(args.prebuilt) / file, out / file)
            return file
        target_dir = slots.get()
        try:
            if failed.is_set():
                return None
            print(f"== {pkg} {driver_id}" + (f" ({target_dir.name})" if jobs > 1 else ""), flush=True)
            env = dict(os.environ, DBINE_DRIVER_VERSION=driver_id, CARGO_TARGET_DIR=str(target_dir))
            cmd = [*cargo_build, "--quiet", "-p", "dbine-plugin-host", "--release", "--target", target, "--no-default-features", "--features", pkg]
            if jobs == 1:
                run(cmd, env=env)
            else:
                # Each build's output as one block, not interleaved with the others'.
                r = subprocess.run(cmd, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
                log = r.stdout.decode("utf-8", "replace")
                print(f"+ {' '.join(cmd)}\n{log}" if log.strip() or r.returncode else "", end="", flush=True)
                if r.returncode:
                    raise subprocess.CalledProcessError(r.returncode, cmd)
            binary = target_dir / target / "release" / f"dbine-plugin-host{exe}"
            with open(binary, "rb") as src, gzip.open(out / file, "wb", compresslevel=9) as dst:
                shutil.copyfileobj(src, dst)
            return file
        except BaseException:
            failed.set()
            raise
        finally:
            slots.put(target_dir)

    with ThreadPoolExecutor(max_workers=jobs) as pool:
        # The first one alone: tools that set themselves up on first use race
        # when several start at once (cargo xwin links its clang-cl: "File
        # exists").
        futures = [pool.submit(build, pkg, driver_id) for pkg, _, driver_id, *_ in todo[:1]]
        wait(futures)
        futures += [pool.submit(build, pkg, driver_id) for pkg, _, driver_id, *_ in todo[1:]]
        wait(futures)
    failures = [(t[0], f.exception()) for t, f in zip(todo, futures) if f.exception()]
    if failures:
        sys.exit("\n".join(["No compiló:", *(f"- {pkg}: {e}" for pkg, e in failures)]))

    if shard:
        files = [f.result() for f in futures]
        (out / "new-files.txt").write_text("".join(f"{f}\n" for f in files), "utf-8", newline="\n")
        print(f"Shard {shard[0]}/{shard[1]}: {len(files)} compilados" + (": " + ", ".join(t[0] for t in todo) if todo else ""))
        return

    published_at = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    for (pkg, version, driver_id, own, shared, entry_min_app), f in zip(todo, futures):
        file = f.result()
        data = (out / file).read_bytes()
        index["drivers"][pkg][driver_id] = {
            "file": file,
            "size": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
            "own_hash": own,
            "shared_hash": shared,
            "manifest": [m for m in manifest if m["package"] == pkg],
            "min_app": entry_min_app,
            "yanked": None,
            "published_at": published_at,
        }
        built.append((pkg, version, file))

    index_file = out / f"index-{target}.json"
    index_file.write_text(json.dumps(index, ensure_ascii=False, indent=1), "utf-8")
    # "\n" on every system: the workflow reads it with xargs (Windows would write "\r\n").
    (out / "new-files.txt").write_text("".join(f"{f}\n" for _, _, f in built), "utf-8", newline="\n")

    if not args.no_catalog:
        hosts, drivers = {}, []
        for pkg in packages:
            driver_id = f"{by_name[f'dbine-driver-{pkg}']['version']}+p{protocol}.e{epoch}"
            e = index["drivers"][pkg].get(driver_id)
            if not e:
                sys.exit(f"sin catálogo: {pkg} {driver_id} no está publicado ni se compiló ahora (--no-catalog para no escribirlo)")
            hosts[pkg] = {"version": driver_id, "file": e["file"], "size": e["size"], "sha256": e["sha256"]}
            drivers.extend(e["manifest"])
        catalog = out / "plugins.json"
        # min_index_seq: the published index's seq. The app refuses an older
        # signed index even on a fresh install (a replayed or rolled-back one).
        min_index_seq = int(index.get("seq") or 0)
        catalog.write_text(json.dumps({"target": target, "base_url": base_url, "hosts": hosts, "drivers": drivers, "min_index_seq": min_index_seq}, ensure_ascii=False, separators=(",", ":")), "utf-8")
        # This code must read what older drivers said about themselves.
        run([str(native_dir / f"dbine-plugin-host{'.exe' if os.name == 'nt' else ''}"), "--check-catalog", str(catalog)])

    lines = [f"### Drivers ({target})", "", f"Protocolo {protocol}, epoch {epoch}.", ""]
    lines += [f"- Compilados: {len(built)}" + (": " + ", ".join(f"{p} {v}" for p, v, _ in built) if built else "")]
    lines += [f"- Reutilizados: {len(reused)}"]
    if built:
        lines += [f"- Piden la app {min(filter(None, (t[5] for t in todo)), key=driver_index.version_key)} o posterior"
                  + (" (algunos más nueva, por su min-app)" if len({t[5] for t in todo}) > 1 else "")]
    if skipped:
        lines += [f"- Sin publicar y fuera de --only: {', '.join(skipped)}"]
    if warnings:
        lines += ["", "#### Revisar", "", *warnings]
    (out / "summary.md").write_text("\n".join(lines) + "\n", "utf-8")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
