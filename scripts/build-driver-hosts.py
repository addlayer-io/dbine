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
  code it shares with other drivers (dbine-driver, dependencies) changed.

Writes to <out-dir>:
- index-<target>.json: the updated index (to publish in the `drivers` release);
- the new hosts' .gz files and new-files.txt, the list of them;
- plugins.json: the catalog the app carries (DBINE_PLUGIN_CATALOG);
- summary.md: what was built, reused and warned.

  scripts/build-driver-hosts.py <target> <out-dir> <base-url> [<published index>]

Cross-compiling (Windows from macOS): CARGO_BUILD="cargo xwin build".
Hosts to build at once: DBINE_DRIVER_JOBS (default: one per four CPUs, up
to three; extra ones build in target/driver-hosts-<n>).
"""

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


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    target, out, base_url = sys.argv[1], Path(sys.argv[2]).resolve(), sys.argv[3]
    published = Path(sys.argv[4]) if len(sys.argv) > 4 and sys.argv[4] else None
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
    manifest = json.loads(run(["cargo", "run", "--quiet", "-p", "dbine-plugin-host", "--release", *native, "--", "--manifest"], capture=True))

    built, reused, errors, warnings, todo = [], [], [], [], []
    for pkg in packages:
        crate = by_name.get(f"dbine-driver-{pkg}")
        if not crate:
            sys.exit(f"no encuentro el crate dbine-driver-{pkg}")
        version = crate["version"]
        driver_id = f"{version}+p{protocol}.e{epoch}"
        own, shared = hashes(crate["id"])
        entries = index["drivers"].setdefault(pkg, {})
        entry = entries.get(driver_id)
        if entry:
            if entry["own_hash"] != own:
                errors.append(f"- **{pkg}**: cambió su código y sigue en la versión {version}. Subí `version` en crates/drivers/{pkg}/Cargo.toml.")
            elif entry["shared_hash"] != shared:
                warnings.append(f"- **{pkg}** {version}: cambió código que comparte (dbine-driver, dependencias). Si le afecta, subí su versión o el epoch.")
            reused.append(f"{pkg} {version}")
            continue
        todo.append((pkg, version, driver_id, own, shared))

    # Before building anything: nothing gets published while one of them fails.
    if errors:
        (out / "summary.md").write_text("### Drivers sin versión nueva\n\n" + "\n".join(errors) + "\n", "utf-8")
        sys.exit("\n".join(["Drivers que cambiaron sin subir su versión:", *errors]))

    # Each host is the same binary with other features, so builds that run at
    # once need their own target dirs (slot 0 keeps the usual one).
    jobs = build_jobs(len(todo))
    base_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    slots = queue.Queue()
    for i in range(jobs):
        slots.put(base_dir if i == 0 else base_dir / f"driver-hosts-{i}")
    failed = threading.Event()

    def build(pkg, driver_id):
        """Build one host and gzip it into `out`; the file name, or None if
        skipped after another build failed."""
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
            file = f"dbine-driver-{pkg}-{driver_id}-{target}.gz"
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
        futures = [pool.submit(build, pkg, driver_id) for pkg, _, driver_id, _, _ in todo]
        wait(futures)
    failures = [(t[0], f.exception()) for t, f in zip(todo, futures) if f.exception()]
    if failures:
        sys.exit("\n".join(["No compiló:", *(f"- {pkg}: {e}" for pkg, e in failures)]))

    for (pkg, version, driver_id, own, shared), f in zip(todo, futures):
        file = f.result()
        data = (out / file).read_bytes()
        index["drivers"][pkg][driver_id] = {
            "file": file,
            "size": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
            "own_hash": own,
            "shared_hash": shared,
            "manifest": [m for m in manifest if m["package"] == pkg],
        }
        built.append((pkg, version, file))

    index_file = out / f"index-{target}.json"
    index_file.write_text(json.dumps(index, ensure_ascii=False, indent=1), "utf-8")
    # "\n" on every system: the workflow reads it with xargs (Windows would write "\r\n").
    (out / "new-files.txt").write_text("".join(f"{f}\n" for _, _, f in built), "utf-8", newline="\n")

    hosts, drivers = {}, []
    for pkg in packages:
        driver_id = f"{by_name[f'dbine-driver-{pkg}']['version']}+p{protocol}.e{epoch}"
        e = index["drivers"][pkg][driver_id]
        hosts[pkg] = {"version": driver_id, "file": e["file"], "size": e["size"], "sha256": e["sha256"]}
        drivers.extend(e["manifest"])
    catalog = out / "plugins.json"
    catalog.write_text(json.dumps({"target": target, "base_url": base_url, "hosts": hosts, "drivers": drivers}, ensure_ascii=False, separators=(",", ":")), "utf-8")
    # This code must read what older drivers said about themselves.
    run([str(native_dir / f"dbine-plugin-host{'.exe' if os.name == 'nt' else ''}"), "--check-catalog", str(catalog)])

    lines = [f"### Drivers ({target})", "", f"Protocolo {protocol}, epoch {epoch}.", ""]
    lines += [f"- Compilados: {len(built)}" + (": " + ", ".join(f"{p} {v}" for p, v, _ in built) if built else "")]
    lines += [f"- Reutilizados: {len(reused)}"]
    if warnings:
        lines += ["", "#### Revisar", "", *warnings]
    (out / "summary.md").write_text("\n".join(lines) + "\n", "utf-8")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
