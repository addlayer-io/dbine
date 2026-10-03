#!/usr/bin/env python3
"""Tests of make-latest-json.py: python3 scripts/test_make_latest_json.py

Keys and signatures are made here in the minisign format the Tauri CLI
writes. With `cargo tauri` installed, one more test signs with the real CLI.
"""

import base64
import hashlib
import importlib.util
import json
import pathlib
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("make_latest_json", HERE / "make-latest-json.py")
mlj = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mlj)


class Key:
    """A minisign key pair, the way `cargo tauri signer` stores it."""

    def __init__(self, seed: bytes, key_id: bytes):
        self.seed = seed
        self.key_id = key_id
        self.public = mlj.ed25519_public(seed)

    def conf_pubkey(self) -> str:
        pub = (
            f"untrusted comment: minisign public key: {self.key_id[::-1].hex().upper()}\n"
            + base64.b64encode(b"Ed" + self.key_id + self.public).decode()
            + "\n"
        )
        return base64.b64encode(pub.encode()).decode()

    def sign(self, data: bytes, trusted: str, prehashed=True) -> str:
        msg = hashlib.blake2b(data, digest_size=64).digest() if prehashed else data
        sig = mlj.ed25519_sign(self.seed, msg)
        global_sig = mlj.ed25519_sign(self.seed, sig + trusted.encode())
        text = (
            "untrusted comment: signature from tauri secret key\n"
            + base64.b64encode((b"ED" if prehashed else b"Ed") + self.key_id + sig).decode()
            + "\n"
            + f"trusted comment: {trusted}\n"
            + base64.b64encode(global_sig).decode()
            + "\n"
        )
        return base64.b64encode(text.encode()).decode()


KEY = Key(bytes(range(32)), bytes.fromhex("0102030405060708"))
OTHER = Key(bytes(range(1, 33)), bytes.fromhex("1112131415161718"))


class Ed25519(unittest.TestCase):
    def test_rfc8032_vector_1(self):
        secret = bytes.fromhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
        public = bytes.fromhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
        sig = bytes.fromhex(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        )
        self.assertEqual(mlj.ed25519_public(secret), public)
        self.assertEqual(mlj.ed25519_sign(secret, b""), sig)
        self.assertTrue(mlj.ed25519_verify(public, b"", sig))
        self.assertFalse(mlj.ed25519_verify(public, b"x", sig))


class Manifest(unittest.TestCase):
    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.conf = self.tmp / "tauri.conf.json"
        self.write_conf(KEY.conf_pubkey())

    def write_conf(self, pubkey):
        self.conf.write_text(json.dumps({"plugins": {"updater": {"pubkey": pubkey}}}))

    def artifact(self, name, data=b"package bytes", version="0.1.4", key=KEY, sig_override=None):
        d = self.tmp / "build" / name
        d.parent.mkdir(parents=True, exist_ok=True)
        d.write_bytes(data)
        sig = sig_override or key.sign(data, f"timestamp:1700000000\tfile:{name}\tversion:{version}")
        (d.parent / (name + ".sig")).write_text(sig)
        return d

    def run_script(self, *artifacts, extra=()):
        out = self.tmp / "dist" / "latest.json"
        argv = ["--version", "0.1.4", "--out", str(out), "--conf", str(self.conf), "--notes-file", str(self.notes())]
        for a in artifacts:
            argv += ["--artifact", a]
        code = mlj.main(argv + list(extra))
        return code, out

    def notes(self):
        p = self.tmp / "notes.md"
        p.write_text("## Novedades\n\n- Se actualiza sola\n")
        return p

    def test_good_manifest(self):
        mac = self.artifact("DBine.app.tar.gz")
        win = self.artifact("DBine_0.1.4_x64-setup.exe", b"MZ installer")
        code, out = self.run_script(f"darwin-aarch64-app={mac}", f"windows-x86_64-nsis={win}")
        self.assertEqual(code, 0)
        m = json.loads(out.read_text())
        self.assertEqual(m["version"], "0.1.4")
        self.assertEqual(m["notes"], "## Novedades\n\n- Se actualiza sola")
        self.assertRegex(m["pub_date"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        self.assertEqual(sorted(m["platforms"]), ["darwin-aarch64-app", "windows-x86_64-nsis"])
        self.assertEqual(
            m["platforms"]["darwin-aarch64-app"]["url"],
            "https://github.com/addlayer-io/dbine/releases/download/v0.1.4/DBine_0.1.4_aarch64.app.tar.gz",
        )
        self.assertEqual(m["platforms"]["darwin-aarch64-app"]["signature"], (mac.parent / "DBine.app.tar.gz.sig").read_text())
        self.assertTrue((out.parent / "DBine_0.1.4_aarch64.app.tar.gz").is_file())
        self.assertTrue((out.parent / "DBine_0.1.4_aarch64.app.tar.gz.sig").is_file())
        files = (out.parent / "upload-files.txt").read_bytes()
        self.assertNotIn(b"\r", files)
        self.assertEqual(len(files.decode().splitlines()), 4)

    def test_unprehashed_signatures_too(self):
        f = self.artifact("DBine_0.1.4_amd64.AppImage")
        f.with_name(f.name + ".sig").write_text(KEY.sign(f.read_bytes(), "timestamp:1\tfile:x\tversion:0.1.4", prehashed=False))
        self.assertEqual(self.run_script(f"linux-x86_64-appimage={f}")[0], 0)

    def test_refuses_the_placeholder(self):
        self.write_conf(mlj.PLACEHOLDER)
        f = self.artifact("DBine.app.tar.gz")
        code, out = self.run_script(f"darwin-aarch64-app={f}")
        self.assertEqual(code, 1)
        self.assertFalse(out.exists())

    def test_refuses_another_key(self):
        f = self.artifact("DBine.app.tar.gz", key=OTHER)
        self.assertEqual(self.run_script(f"darwin-aarch64-app={f}")[0], 1)

    def test_refuses_another_key_with_the_same_id(self):
        impostor = Key(bytes(range(2, 34)), KEY.key_id)
        f = self.artifact("DBine.app.tar.gz", key=impostor)
        self.assertEqual(self.run_script(f"darwin-aarch64-app={f}")[0], 1)

    def test_refuses_another_version(self):
        f = self.artifact("DBine.app.tar.gz", version="0.1.3")
        self.assertEqual(self.run_script(f"darwin-aarch64-app={f}")[0], 1)

    def test_refuses_a_signature_without_version(self):
        data = b"package bytes"
        sig = KEY.sign(data, "timestamp:1700000000\tfile:DBine.app.tar.gz")
        f = self.artifact("DBine.app.tar.gz", data, sig_override=sig)
        self.assertEqual(self.run_script(f"darwin-aarch64-app={f}")[0], 1)

    def test_refuses_a_tampered_file(self):
        f = self.artifact("DBine.app.tar.gz")
        f.write_bytes(b"package bytes, changed")
        self.assertEqual(self.run_script(f"darwin-aarch64-app={f}")[0], 1)

    def test_refuses_a_tampered_trusted_comment(self):
        data = b"package bytes"
        good = base64.b64decode(KEY.sign(data, "timestamp:1\tfile:x\tversion:0.1.3")).decode()
        forged = good.replace("version:0.1.3", "version:0.1.4")
        f = self.artifact("DBine.app.tar.gz", data, sig_override=base64.b64encode(forged.encode()).decode())
        self.assertEqual(self.run_script(f"darwin-aarch64-app={f}")[0], 1)

    def test_refuses_unknown_platform_keys(self):
        f = self.artifact("DBine_0.1.4_amd64.deb")
        self.assertEqual(self.run_script(f"linux-x86_64={f}")[0], 1)
        self.assertEqual(self.run_script(f"linux-x86_64-deb={f}")[0], 1)

    def test_merge_keeps_published_platforms(self):
        mac = self.artifact("DBine.app.tar.gz")
        code, out = self.run_script(f"darwin-aarch64-app={mac}")
        self.assertEqual(code, 0)
        first = json.loads(out.read_text())
        prev = self.tmp / "prev.json"
        shutil.copyfile(out, prev)
        linux = self.artifact("DBine_0.1.4_amd64.AppImage", b"ELF appimage")
        code, out = self.run_script(f"linux-x86_64-appimage={linux}", extra=["--merge", str(prev)])
        self.assertEqual(code, 0)
        m = json.loads(out.read_text())
        self.assertEqual(sorted(m["platforms"]), ["darwin-aarch64-app", "linux-x86_64-appimage"])
        self.assertEqual(m["pub_date"], first["pub_date"])
        # Another version's manifest isn't merged.
        prev.write_text(json.dumps({**first, "version": "0.1.3"}))
        code, out = self.run_script(f"linux-x86_64-appimage={linux}", extra=["--merge", str(prev)])
        self.assertEqual(sorted(json.loads(out.read_text())["platforms"]), ["linux-x86_64-appimage"])

    def test_check_uploaded(self):
        mac = self.artifact("DBine.app.tar.gz")
        size = mac.stat().st_size
        uploaded = {"assets": [{"name": "DBine_0.1.4_aarch64.app.tar.gz", "size": size}]}
        with mock.patch.object(mlj, "gh_json", return_value=uploaded):
            self.assertEqual(self.run_script(f"darwin-aarch64-app={mac}", extra=["--check-uploaded"])[0], 0)
        with mock.patch.object(mlj, "gh_json", return_value={"assets": []}):
            code, out = self.run_script(f"darwin-aarch64-app={mac}", extra=["--check-uploaded"])
            self.assertEqual(code, 1)
        wrong = {"assets": [{"name": "DBine_0.1.4_aarch64.app.tar.gz", "size": size + 1}]}
        with mock.patch.object(mlj, "gh_json", return_value=wrong):
            self.assertEqual(self.run_script(f"darwin-aarch64-app={mac}", extra=["--check-uploaded"])[0], 1)

    @unittest.skipUnless(shutil.which("cargo") and subprocess.run(
        ["cargo", "tauri", "--version"], capture_output=True).returncode == 0, "cargo tauri not installed")
    def test_signatures_of_the_tauri_cli(self):
        key = self.tmp / "updtest.key"
        subprocess.run(["cargo", "tauri", "signer", "generate", "--ci", "-p", "", "-w", str(key)],
                       check=True, capture_output=True)
        self.write_conf(key.with_name("updtest.key.pub").read_text().strip())
        f = self.tmp / "build" / "DBine.app.tar.gz"
        f.parent.mkdir(parents=True, exist_ok=True)
        f.write_bytes(b"x" * 300_000)
        env = {"PATH": __import__("os").environ["PATH"], "HOME": __import__("os").environ.get("HOME", "")}
        subprocess.run(["cargo", "tauri", "signer", "sign", "-f", str(key), "-p", "", "--app-version", "0.1.4", str(f)],
                       check=True, capture_output=True, env=env)
        self.assertEqual(self.run_script(f"darwin-aarch64-app={f}")[0], 0)
        f.write_bytes(b"y" * 300_000)
        self.assertEqual(self.run_script(f"darwin-aarch64-app={f}")[0], 1)


if __name__ == "__main__":
    unittest.main()
