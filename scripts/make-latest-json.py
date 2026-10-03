#!/usr/bin/env python3
"""Build the in-app updater manifest (`latest.json`) of a DBine release.

The app reads https://github.com/addlayer-io/dbine/releases/latest/download/latest.json
(tauri-plugin-updater, docs/actualizaciones.md). For each platform it lists
the signed update package and its minisign signature (the `.sig` the
`cargo tauri build --config src-tauri/tauri.updater.conf.json` build writes
next to it).

Usage:

    make-latest-json.py --version 0.1.4 --repo addlayer-io/dbine --out dist/latest.json \\
      [--merge prev/latest.json] [--notes-file notes.md] \\
      --artifact darwin-aarch64-app=target/aarch64-apple-darwin/release/bundle/macos/DBine.app.tar.gz \\
      --artifact darwin-x86_64-app=target/x86_64-apple-darwin/release/bundle/macos/DBine.app.tar.gz \\
      --artifact windows-x86_64-nsis=target/x86_64-pc-windows-msvc/release/bundle/nsis/DBine_0.1.4_x64-setup.exe \\
      --artifact linux-x86_64-appimage=linux-out/DBine_0.1.4_amd64.AppImage \\
      [--check-uploaded]

What it does:

1. Refuses to go on while `pubkey` in src-tauri/tauri.conf.json is the
   placeholder: a release must never point at packages nobody can verify.
2. Verifies every `<artifact>.sig` against that public key (key id, the
   Ed25519 signature of the file, the global signature over the trusted
   comment) and requires `version:<version>` in its trusted comment, as the
   app does (`requireSignedVersion`).
3. Copies each artifact and its `.sig` next to `--out` under the release
   asset name (both macOS packages are called `DBine.app.tar.gz`).
4. Writes the manifest. Platform keys always name the installer
   (`windows-x86_64-nsis`, `linux-x86_64-appimage`…), so a deb, rpm or MSI
   install can never pick a package of another kind.
5. `--merge` keeps the platforms an earlier run already published for the
   same version (platforms go up one by one), and their `pub_date`.
6. `--check-uploaded` asks `gh release view` that every package the
   manifest points at is already in the release (with the same size).

It writes `upload-files.txt` (the copied files, one per line) next to the
manifest and never prints environment variables or keys. Only the
standard library: the signature check is RFC 8032 Ed25519 in Python.
"""

import argparse
import base64
import datetime
import hashlib
import json
import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_CONF = ROOT / "src-tauri" / "tauri.conf.json"
PLACEHOLDER = "DBINE_UPDATER_PUBKEY_PLACEHOLDER"

# Platform key -> release asset name ({v} is the version).
ASSETS = {
    "darwin-aarch64-app": "DBine_{v}_aarch64.app.tar.gz",
    "darwin-x86_64-app": "DBine_{v}_x64.app.tar.gz",
    "windows-x86_64-nsis": "DBine_{v}_x64-setup.exe",
    "linux-x86_64-appimage": "DBine_{v}_amd64.AppImage",
}


class Fail(Exception):
    """A reason to stop, said in one line."""


# --- Ed25519 (RFC 8032, verification and, for the tests, signing) ----------

_P = 2**255 - 19
_L = 2**252 + 27742317777372353535851937790883648493
_D = -121665 * pow(121666, _P - 2, _P) % _P
_I = pow(2, (_P - 1) // 4, _P)


def _inv(x):
    return pow(x, _P - 2, _P)


def _add(a, b):
    # Extended coordinates (X, Y, Z, T).
    x1, y1, z1, t1 = a
    x2, y2, z2, t2 = b
    A = (y1 - x1) * (y2 - x2) % _P
    B = (y1 + x1) * (y2 + x2) % _P
    C = 2 * t1 * t2 * _D % _P
    D = 2 * z1 * z2 % _P
    E, F, G, H = B - A, D - C, D + C, B + A
    return (E * F % _P, G * H % _P, F * G % _P, E * H % _P)


def _mul(s, p):
    q = (0, 1, 1, 0)
    while s > 0:
        if s & 1:
            q = _add(q, p)
        p = _add(p, p)
        s >>= 1
    return q


def _equal(a, b):
    x1, y1, z1, _ = a
    x2, y2, z2, _ = b
    return (x1 * z2 - x2 * z1) % _P == 0 and (y1 * z2 - y2 * z1) % _P == 0


def _recover_x(y, sign):
    if y >= _P:
        return None
    x2 = (y * y - 1) * _inv(_D * y * y + 1)
    if x2 == 0:
        return None if sign else 0
    x = pow(x2, (_P + 3) // 8, _P)
    if (x * x - x2) % _P != 0:
        x = x * _I % _P
    if (x * x - x2) % _P != 0:
        return None
    if (x & 1) != sign:
        x = _P - x
    return x


_GY = 4 * _inv(5) % _P
_GX = _recover_x(_GY, 0)
_G = (_GX, _GY, 1, _GX * _GY % _P)


def _compress(p):
    x, y, z, _ = p
    zi = _inv(z)
    x, y = x * zi % _P, y * zi % _P
    return int.to_bytes(y | ((x & 1) << 255), 32, "little")


def _decompress(b):
    if len(b) != 32:
        return None
    y = int.from_bytes(b, "little")
    sign = y >> 255
    y &= (1 << 255) - 1
    x = _recover_x(y, sign)
    if x is None:
        return None
    return (x, y, 1, x * y % _P)


def _h(m):
    return int.from_bytes(hashlib.sha512(m).digest(), "little")


def ed25519_verify(public, msg, sig):
    if len(public) != 32 or len(sig) != 64:
        return False
    a = _decompress(public)
    r = _decompress(sig[:32])
    if a is None or r is None:
        return False
    s = int.from_bytes(sig[32:], "little")
    if s >= _L:
        return False
    k = _h(sig[:32] + public + msg) % _L
    return _equal(_mul(s, _G), _add(r, _mul(k, a)))


def ed25519_public(secret):
    h = hashlib.sha512(secret).digest()
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 8
    a |= 1 << 254
    return _compress(_mul(a, _G))


def ed25519_sign(secret, msg):
    h = hashlib.sha512(secret).digest()
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 8
    a |= 1 << 254
    public = _compress(_mul(a, _G))
    r = _h(h[32:] + msg) % _L
    rs = _compress(_mul(r, _G))
    s = (r + _h(rs + public + msg) * a) % _L
    return rs + int.to_bytes(s, 32, "little")


# --- minisign ---------------------------------------------------------------


def _b64(text, what):
    try:
        return base64.b64decode(text.strip(), validate=True)
    except Exception:
        raise Fail(f"{what}: not valid base64")


def parse_pubkey(conf_value):
    """The `pubkey` of tauri.conf.json (base64 of a minisign .pub file) ->
    (key id, Ed25519 public key)."""
    text = _b64(conf_value, "pubkey").decode("utf-8", "replace")
    lines = [l for l in text.splitlines() if l.strip()]
    if len(lines) < 2:
        raise Fail("pubkey: not a minisign public key")
    raw = _b64(lines[1], "pubkey")
    if len(raw) != 42 or raw[:2] != b"Ed":
        raise Fail("pubkey: not a minisign Ed25519 public key")
    return raw[2:10], raw[10:]


def parse_signature(sig_file_text):
    """A tauri `.sig` (base64 of a minisign signature file) -> its parts."""
    text = _b64(sig_file_text, "signature").decode("utf-8", "replace")
    lines = text.splitlines()
    if len(lines) < 4 or not lines[2].startswith("trusted comment: "):
        raise Fail("signature: not a minisign signature")
    raw = _b64(lines[1], "signature")
    if len(raw) != 74 or raw[:2] not in (b"Ed", b"ED"):
        raise Fail("signature: unknown minisign algorithm")
    trusted = lines[2][len("trusted comment: "):]
    global_sig = _b64(lines[3], "signature")
    return {
        "prehashed": raw[:2] == b"ED",
        "key_id": raw[2:10],
        "sig": raw[10:],
        "trusted": trusted,
        "global": global_sig,
    }


def _blake2b_file(path):
    h = hashlib.blake2b(digest_size=64)
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.digest()


def signed_version(trusted):
    for field in trusted.split("\t"):
        if field.startswith("version:"):
            return field[len("version:"):]
    return None


def verify_artifact(path, sig_text, key_id, public, version):
    """Raise `Fail` unless `sig_text` is a valid signature of the file at
    `path` by this key, bound to `version`."""
    sig = parse_signature(sig_text)
    if sig["key_id"] != key_id:
        raise Fail(
            f"{path.name}: signed with key {sig['key_id'][::-1].hex().upper()}, "
            f"but tauri.conf.json has {key_id[::-1].hex().upper()}"
        )
    if sig["prehashed"]:
        message = _blake2b_file(path)
    else:
        message = path.read_bytes()
    if not ed25519_verify(public, message, sig["sig"]):
        raise Fail(f"{path.name}: the signature doesn't match the file")
    if not ed25519_verify(public, sig["sig"] + sig["trusted"].encode("utf-8"), sig["global"]):
        raise Fail(f"{path.name}: the trusted comment's signature is invalid")
    signed = signed_version(sig["trusted"])
    if signed != version:
        raise Fail(f"{path.name}: signed for version {signed or '(none)'}, not {version}")


# --- the manifest -----------------------------------------------------------


def read_pubkey(conf_path):
    conf = json.loads(pathlib.Path(conf_path).read_text(encoding="utf-8"))
    key = (((conf.get("plugins") or {}).get("updater") or {}).get("pubkey") or "").strip()
    if not key or key == PLACEHOLDER:
        raise Fail(
            f"{conf_path}: the updater pubkey is the placeholder. Set the real public key "
            "(~/.tauri/dbine-updater.key.pub) before publishing updater artifacts."
        )
    return key


def gh_json(args):
    out = subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout
    return json.loads(out)


def release_notes(version, repo):
    try:
        return (gh_json(["release", "view", f"v{version}", "--repo", repo, "--json", "body"]).get("body") or "").strip()
    except Exception:
        return ""


def check_uploaded(manifest, version, repo, local_sizes):
    try:
        assets = gh_json(["release", "view", f"v{version}", "--repo", repo, "--json", "assets"])["assets"]
    except Exception as e:
        raise Fail(f"gh release view v{version}: {e}")
    sizes = {a["name"]: a.get("size") for a in assets}
    for key, entry in manifest["platforms"].items():
        name = entry["url"].rsplit("/", 1)[-1]
        if name not in sizes:
            raise Fail(f"{key}: {name} isn't uploaded to v{version} yet")
        if name in local_sizes and sizes[name] != local_sizes[name]:
            raise Fail(f"{key}: {name} in the release is {sizes[name]} bytes, the local one {local_sizes[name]}")


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).strftime("%Y-%m-%dT%H:%M:%SZ")


def build(args):
    version = args.version.strip().lstrip("v")
    pubkey = read_pubkey(args.conf)
    key_id, public = parse_pubkey(pubkey)
    out = pathlib.Path(args.out)
    out_dir = out.parent
    out_dir.mkdir(parents=True, exist_ok=True)

    platforms = {}
    pub_date = None
    if args.merge and pathlib.Path(args.merge).is_file():
        prev = json.loads(pathlib.Path(args.merge).read_text(encoding="utf-8"))
        if str(prev.get("version", "")).lstrip("v") == version:
            platforms.update(prev.get("platforms") or {})
            pub_date = prev.get("pub_date")
        else:
            print(f"--merge: {args.merge} is for {prev.get('version')}, not {version}: starting over")

    if not args.artifact and not platforms:
        raise Fail("nothing to publish: pass --artifact KEY=PATH")

    copied = []
    local_sizes = {}
    for spec in args.artifact or []:
        key, sep, path = spec.partition("=")
        if not sep or key not in ASSETS:
            raise Fail(f"--artifact {spec}: the key must be one of {', '.join(ASSETS)}")
        path = pathlib.Path(path)
        sig_path = path.with_name(path.name + ".sig")
        if not path.is_file():
            raise Fail(f"{key}: {path} doesn't exist")
        if not sig_path.is_file():
            raise Fail(f"{key}: {sig_path} doesn't exist (build with --config src-tauri/tauri.updater.conf.json)")
        sig_text = sig_path.read_text(encoding="utf-8").strip()
        verify_artifact(path, sig_text, key_id, public, version)
        asset = ASSETS[key].format(v=version)
        dest = out_dir / asset
        if path.resolve() != dest.resolve():
            shutil.copyfile(path, dest)
        dest_sig = out_dir / (asset + ".sig")
        if sig_path.resolve() != dest_sig.resolve():
            shutil.copyfile(sig_path, dest_sig)
        copied += [dest, dest_sig]
        local_sizes[asset] = dest.stat().st_size
        platforms[key] = {
            "signature": sig_text,
            "url": f"https://github.com/{args.repo}/releases/download/v{version}/{asset}",
        }
        print(f"{key}: {asset} ({local_sizes[asset]} bytes), signature verified")

    if args.notes_file:
        notes = pathlib.Path(args.notes_file).read_text(encoding="utf-8").strip()
    else:
        notes = release_notes(version, args.repo)

    manifest = {
        "version": version,
        "notes": notes,
        "pub_date": pub_date or utc_now(),
        "platforms": dict(sorted(platforms.items())),
    }
    if args.check_uploaded:
        check_uploaded(manifest, version, args.repo, local_sizes)
    with open(out, "w", encoding="utf-8", newline="\n") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=2)
        f.write("\n")
    with open(out_dir / "upload-files.txt", "w", encoding="utf-8", newline="\n") as f:
        for p in copied:
            f.write(f"{p}\n")
    print(f"{out}: {', '.join(manifest['platforms'])}")
    return manifest


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--version", required=True)
    ap.add_argument("--repo", default="addlayer-io/dbine")
    ap.add_argument("--out", required=True)
    ap.add_argument("--merge")
    ap.add_argument("--notes-file")
    ap.add_argument("--artifact", action="append", metavar="KEY=PATH")
    ap.add_argument("--check-uploaded", action="store_true")
    ap.add_argument("--conf", default=str(DEFAULT_CONF), help=argparse.SUPPRESS)
    args = ap.parse_args(argv)
    try:
        build(args)
    except Fail as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
