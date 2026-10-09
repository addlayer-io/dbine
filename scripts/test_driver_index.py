#!/usr/bin/env python3
"""Tests of driver_index.py and the checks of build-driver-hosts.py:
python3 scripts/test_driver_index.py

Signatures are made with a throwaway key, in Python or (with `cargo tauri`
installed) with the real `tauri signer`; never with the updater key.
"""

import importlib.util
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from unittest import mock

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import driver_index as di  # noqa: E402
from test_make_latest_json import KEY, OTHER  # noqa: E402

spec = importlib.util.spec_from_file_location("build_driver_hosts", HERE / "build-driver-hosts.py")
bdh = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bdh)


def entry(file, min_app=None, yanked=None, ids=("pg",), sha="s"):
    return {"file": file, "sha256": sha, "min_app": min_app, "yanked": yanked, "manifest": [{"package": "pkg", "info": {"id": i}} for i in ids]}


class Versions(unittest.TestCase):
    def test_semver_order(self):
        order = ["0.1.2", "0.1.10-alpha", "0.1.10-alpha.2", "0.1.10-beta", "0.1.10", "0.2.0", "1.0.0"]
        self.assertEqual(sorted(reversed(order), key=di.version_key), order)
        self.assertEqual(di.version_key("0.1.10+build"), di.version_key("0.1.10"))
        with self.assertRaises(di.Fail):
            di.version_key("v0.1")

    def test_parse_id(self):
        self.assertEqual(di.parse_id("0.1.10+p1.e3"), ("0.1.10", 1, 3))
        with self.assertRaises(di.Fail):
            di.parse_id("0.1.10")

    def test_newest_skips_yanked_and_other_lines(self):
        entries = {"0.1.8+p1.e3": entry("a"), "0.1.10+p1.e3": entry("b", yanked="crash"), "0.1.9+p1.e3": entry("c"), "0.2.0+p1.e4": entry("d")}
        self.assertEqual(di.newest(entries, 1, 3), "0.1.9+p1.e3")
        self.assertEqual(di.newest(entries, 1, 3, skip_yanked=False), "0.1.10+p1.e3")
        self.assertIsNone(di.newest(entries, 2, 3))


class Resolve(unittest.TestCase):
    ENTRIES = {
        "0.1.8+p1.e3": entry("8"),
        "0.1.9+p1.e3": entry("9", min_app="0.1.9"),
        "0.1.10+p1.e3": entry("10", min_app="0.1.9", yanked="se cuelga"),
        "0.1.11+p1.e3": entry("11", min_app="0.2.0"),
        "0.1.12+p1.e4": entry("12"),
    }

    def test_table(self):
        cases = [
            # floor, app, expected
            ("0.1.8+p1.e3", "0.1.8", "0.1.8+p1.e3"),  # 0.1.9 needs app 0.1.9
            ("0.1.8+p1.e3", "0.1.9", "0.1.9+p1.e3"),  # 0.1.10 yanked, 0.1.11 needs 0.2.0
            ("0.1.8+p1.e3", "0.2.0", "0.1.11+p1.e3"),
            ("0.1.9+p1.e4", "0.3.0", "0.1.12+p1.e4"),  # only its epoch
            ("0.1.20+p1.e3", "0.3.0", "0.1.20+p1.e3"),  # never below the floor
            ("0.1.8+p2.e3", "0.3.0", "0.1.8+p2.e3"),  # other protocol: nothing
        ]
        for floor, app, want in cases:
            with self.subTest(floor=floor, app=app):
                self.assertEqual(di.resolve(self.ENTRIES, floor, app), want)

    def test_empty_index(self):
        self.assertEqual(di.resolve({}, "0.1.8+p1.e3", "0.1.9"), "0.1.8+p1.e3")


class MinApp(unittest.TestCase):
    def test_oldest_matching_release(self):
        released = [("0.1.9", "B"), ("0.1.8", "B"), ("0.1.7", "A"), ("0.1.6", "B"), ("0.1.5", "A")]
        self.assertEqual(di.auto_min_app("B", released), "0.1.6")
        self.assertEqual(di.auto_min_app("A", released), "0.1.5")

    def test_driver_release_needs_a_released_app(self):
        self.assertIsNone(di.auto_min_app("C", [("0.1.9", "B")]))

    def test_app_release_is_the_floor(self):
        self.assertEqual(di.auto_min_app("C", [("0.1.9", "B")], app_version="0.2.0"), "0.2.0")
        self.assertEqual(di.auto_min_app("B", [("0.1.9", "B"), ("0.1.8", "A")], app_version="0.2.0"), "0.1.9")

    def test_metadata_only_raises(self):
        self.assertEqual(di.raise_min_app("0.1.6", "0.1.9"), "0.1.9")
        self.assertEqual(di.raise_min_app("0.1.9", "0.1.6"), "0.1.9")
        self.assertEqual(di.raise_min_app("0.1.9", None), "0.1.9")
        self.assertEqual(di.raise_min_app("0.1.9", "0.1.10"), "0.1.10")


class Git(unittest.TestCase):
    """wire_hash and min_app over a throwaway repository with app tags."""

    def setUp(self):
        self.root = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)
        self.git("init", "-q")
        self.git("config", "user.email", "t@example.com")
        self.git("config", "user.name", "t")
        self.git("config", "core.autocrlf", "false")

    def git(self, *a):
        subprocess.run(["git", *a], cwd=self.root, check=True, capture_output=True)

    def write(self, rel, data):
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)

    def release(self, version, proto):
        self.write("crates/dbine-plugin/src/proto.rs", proto)
        self.write("crates/dbine-driver/src/lib.rs", b"pub trait Driver {}\n")
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", version)
        self.git("tag", f"v{version}")

    def test_wire_hash_tree_equals_rev(self):
        self.release("0.1.0", b"const A: u32 = 1;\n")
        self.write("crates/dbine-plugin/src/testdata/x.json", b"{}")
        self.write("crates/dbine-plugin/src/NOTES.md", b"notes")
        self.assertEqual(di.wire_hash(None, self.root), di.wire_hash("v0.1.0", self.root))
        # A Windows checkout (CRLF) hashes the same.
        self.write("crates/dbine-plugin/src/proto.rs", b"const A: u32 = 1;\r\n")
        self.assertEqual(di.wire_hash(None, self.root), di.wire_hash("v0.1.0", self.root))
        self.write("crates/dbine-plugin/src/proto.rs", b"const A: u32 = 2;\n")
        self.assertNotEqual(di.wire_hash(None, self.root), di.wire_hash("v0.1.0", self.root))

    def test_min_app_from_tags(self):
        self.release("0.1.0", b"1")
        self.release("0.1.1", b"2")
        self.release("0.1.2", b"2")
        self.release("0.1.3", b"2")
        # HEAD is v0.1.3: oldest with the same code is 0.1.1.
        self.assertEqual(di.min_app_from_git(5, root=self.root), "0.1.1")
        # Only the last two releases count.
        self.assertEqual(di.min_app_from_git(2, root=self.root), "0.1.2")
        # Changed since: a driver release can't, an app release can.
        self.write("crates/dbine-plugin/src/proto.rs", b"3")
        self.assertIsNone(di.min_app_from_git(5, root=self.root))
        self.assertEqual(di.min_app_from_git(5, "0.1.4", self.root), "0.1.4")
        # The tag being released is HEAD even if an older commit had it.
        self.assertEqual(di.min_app_from_git(5, "0.1.3", self.root), "0.1.3")


class Merge(unittest.TestCase):
    def local(self):
        return {"target": "t", "drivers": {"pg": {"0.1.8+p1.e3": entry("old.gz", sha="o"), "0.1.9+p1.e3": entry("new.gz", sha="n")}}}

    def test_adds_only_this_builds_files(self):
        current = {"target": "t", "seq": 5, "drivers": {"my": {"0.1.1+p1.e3": entry("my.gz")}}}
        self.assertEqual(di.merge(current, self.local(), ["new.gz"]), 1)
        self.assertEqual(sorted(current["drivers"]["pg"]), ["0.1.9+p1.e3"])
        self.assertIn("my", current["drivers"])  # what another run published stays
        # Again (a retry): nothing to add.
        self.assertEqual(di.merge(current, self.local(), ["new.gz"]), 0)

    def test_published_id_never_changes(self):
        current = {"target": "t", "drivers": {"pg": {"0.1.9+p1.e3": entry("new.gz", sha="other")}}}
        with self.assertRaises(di.Fail):
            di.merge(current, self.local(), ["new.gz"])

    def test_drop(self):
        index = self.local()
        self.assertEqual(di.drop(index, {"old.gz"}), 1)
        self.assertEqual(list(index["drivers"]["pg"]), ["0.1.9+p1.e3"])

    def test_stamp_seq_always_grows(self):
        self.assertEqual(di.stamp({"target": "t", "drivers": {}}, now=100)["seq"], 100)
        self.assertEqual(di.stamp({"target": "t", "seq": 500, "drivers": {}}, now=100)["seq"], 501)
        s = di.stamp({"target": "t", "seq": 1, "drivers": {"a": {}}, "extra": 1}, now=100)
        self.assertEqual(list(s), ["target", "schema", "seq", "drivers", "extra"])
        self.assertEqual(s["schema"], 2)


FAKE_SIGNER = textwrap.dedent(
    """
    import base64, hashlib, os, sys
    sys.path.insert(0, {here!r})
    from test_make_latest_json import KEY, OTHER
    key = OTHER if os.environ.get("FAKE_OTHER_KEY") else KEY
    path = sys.argv[-1]
    data = open(path, "rb").read()
    open(path + ".sig", "w").write(key.sign(data, "timestamp:1\\tfile:" + os.path.basename(path)))
    """
)


class Signing(unittest.TestCase):
    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.conf = self.tmp / "tauri.conf.json"
        self.conf.write_text(json.dumps({"plugins": {"updater": {"pubkey": KEY.conf_pubkey()}}}))
        signer = self.tmp / "fake_signer.py"
        signer.write_text(FAKE_SIGNER.format(here=str(HERE)))
        self.signer = f"{sys.executable} {signer}"
        self.index = self.tmp / "index-t.json"
        self.index.write_bytes(di.dump(di.stamp({"target": "t", "drivers": {}})))
        self.env = mock.patch.dict(os.environ, {"TAURI_SIGNING_PRIVATE_KEY": "throwaway"})
        self.env.start()
        self.addCleanup(self.env.stop)

    def test_sign_and_verify(self):
        di.sign(self.index, self.signer, self.conf)
        di.verify(self.index, self.conf)

    def test_tampered_index(self):
        di.sign(self.index, self.signer, self.conf)
        self.index.write_bytes(self.index.read_bytes().replace(b'"t"', b'"u"'))
        with self.assertRaises(di.Fail):
            di.verify(self.index, self.conf)

    def test_other_key_is_refused(self):
        with mock.patch.dict(os.environ, {"FAKE_OTHER_KEY": "1"}):
            with self.assertRaises(di.Fail):
                di.sign(self.index, self.signer, self.conf)

    def test_no_key_no_signing(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(di.Fail):
                di.sign(self.index, self.signer, self.conf)

    @unittest.skipUnless(shutil.which("cargo-tauri"), "cargo tauri no está instalado")
    def test_real_tauri_signer(self):
        key = self.tmp / "throwaway.key"
        subprocess.run(["cargo", "tauri", "signer", "generate", "--ci", "-p", "pw", "-w", str(key)], check=True, capture_output=True)
        self.conf.write_text(json.dumps({"plugins": {"updater": {"pubkey": key.with_suffix(".key.pub").read_text().strip()}}}))
        # The key itself, as the CI secret holds it.
        env = {"TAURI_SIGNING_PRIVATE_KEY": key.read_text().strip(), "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": "pw"}
        with mock.patch.dict(os.environ, env):
            di.sign(self.index, "cargo tauri signer sign", self.conf)
        di.verify(self.index, self.conf)


class UpdatePublished(unittest.TestCase):
    """The download -> change -> sign -> upload loop, with a fake release."""

    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.published = {"target": "t", "seq": 10, "drivers": {"other": {"1.0.0+p1.e3": entry("other.gz")}}}
        self.uploads = 0
        self.overwrite_once = None

        def download(target, directory):
            return json.loads(json.dumps(self.published)) if self.published else None

        def gh(*args):
            if args[:2] == ("release", "upload"):
                self.uploads += 1
                self.published = json.loads(pathlib.Path(args[3]).read_text())
                if self.overwrite_once:
                    # Another run, which read the index before us, uploads after us.
                    self.published, self.overwrite_once = self.overwrite_once, None
            return ""

        for name, fake in (("download", download), ("gh", gh), ("sign", lambda *a, **k: None)):
            p = mock.patch.object(di, name, fake)
            p.start()
            self.addCleanup(p.stop)
        p = mock.patch.object(di.time, "sleep", lambda s: None)
        p.start()
        self.addCleanup(p.stop)

    def local(self):
        return {"target": "t", "drivers": {"pg": {"0.1.9+p1.e3": entry("pg.gz", min_app="0.1.9")}}}

    def test_merges_signs_and_uploads(self):
        di.update_published("t", lambda i: di.merge(i, self.local(), ["pg.gz"]), self.tmp)
        self.assertEqual(self.uploads, 1)
        self.assertEqual(sorted(self.published["drivers"]), ["other", "pg"])
        self.assertGreater(self.published["seq"], 10)
        self.assertEqual(self.published["schema"], 2)

    def test_lost_write_is_redone(self):
        self.overwrite_once = {"target": "t", "seq": 11, "drivers": {"other": {}, "mysql": {"0.2.0+p1.e3": entry("my.gz")}}}
        di.update_published("t", lambda i: di.merge(i, self.local(), ["pg.gz"]), self.tmp)
        self.assertEqual(self.uploads, 2)
        self.assertEqual(sorted(self.published["drivers"]), ["mysql", "other", "pg"])

    def test_dry_run_uploads_nothing(self):
        index = di.update_published("t", lambda i: di.merge(i, self.local(), ["pg.gz"]), self.tmp, dry_run=True)
        self.assertEqual(self.uploads, 0)
        self.assertIn("pg", index["drivers"])
        self.assertTrue((self.tmp / "out" / "index-t.json").is_file())

    def test_first_index_of_a_target(self):
        self.published = None
        di.update_published("t", lambda i: di.merge(i, self.local(), ["pg.gz"]), self.tmp)
        self.assertEqual(list(self.published["drivers"]), ["pg"])

    def test_publish_command(self):
        build = self.tmp / "build"
        build.mkdir()
        (build / "index-t.json").write_text(json.dumps(self.local()))
        (build / "new-files.txt").write_text("pg.gz\n")
        self.assertEqual(di.main(["publish", "t", str(build), "--settle", "0"]), 0)
        self.assertIn("pg", self.published["drivers"])
        (build / "index-t.json").write_text(json.dumps({"target": "u", "drivers": {}}))
        self.assertEqual(di.main(["publish", "t", str(build)]), 1)


class BuildChecks(unittest.TestCase):
    ENTRIES = {
        "0.1.8+p1.e3": entry("a", ids=("pg", "redshift")),
        "0.1.9+p1.e3": entry("b", ids=("pg", "redshift", "cockroach")),
        "0.1.10+p1.e3": entry("c", ids=("pg",), yanked="mal"),
        "0.5.0+p1.e2": entry("d", ids=("old",)),
    }

    def test_good_version(self):
        self.assertEqual(bdh.check_new("pg", "0.1.11", self.ENTRIES, 1, 3, ["pg", "redshift", "cockroach", "nuevo"]), [])

    def test_version_must_exceed_published_even_yanked(self):
        errors = bdh.check_new("pg", "0.1.10", {k: v for k, v in self.ENTRIES.items() if k != "0.1.10+p1.e3"} | {"0.1.10+p1.e3": entry("c", ids=("pg", "redshift", "cockroach"), yanked="mal")}, 1, 3, ["pg", "redshift", "cockroach"])
        self.assertEqual(len(errors), 1)
        self.assertIn("no supera", errors[0])
        self.assertTrue(bdh.check_new("pg", "0.1.9-rc1", self.ENTRIES, 1, 3, ["pg", "redshift", "cockroach"]))

    def test_engine_ids_cannot_disappear(self):
        # Against the newest version that isn't yanked (0.1.9).
        errors = bdh.check_new("pg", "0.1.11", self.ENTRIES, 1, 3, ["pg", "redshift"])
        self.assertEqual(len(errors), 1)
        self.assertIn("cockroach", errors[0])

    def test_new_epoch_starts_clean(self):
        self.assertEqual(bdh.check_new("pg", "0.1.0", self.ENTRIES, 1, 4, ["x"]), [])

    def test_only_flag(self):
        args = bdh.parse_args(["t", "out", "url", "idx.json", "--only", "postgres,mysql", "--only", "redis", "--no-catalog"])
        self.assertEqual(args.only, {"postgres", "mysql", "redis"})
        self.assertTrue(args.no_catalog)
        args = bdh.parse_args(["t", "out", "url"])
        self.assertEqual((args.only, args.no_catalog, args.published), (set(), False, ""))


if __name__ == "__main__":
    unittest.main()
