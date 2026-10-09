#!/usr/bin/env python3
"""Tests of prune-driver-assets.py: python3 scripts/test_prune.py"""

import datetime
import importlib.util
import pathlib
import subprocess
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("prune", HERE / "prune-driver-assets.py")
prune = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prune)

T = "x86_64-unknown-linux-gnu"
NOW = datetime.datetime(2026, 10, 9, 12, tzinfo=datetime.timezone.utc)


def e(pkg, driver_id, target=T, min_app=None, yanked=None):
    return {"file": prune.file_name(pkg, driver_id, target), "min_app": min_app, "yanked": yanked}


def index(target, entries):
    return {"target": target, "drivers": {pkg: {i: e(pkg, i, target, **kw) for i, kw in ids.items()} for pkg, ids in entries.items()}}


def names(keep, target=T):
    return {n for n in keep if n.endswith(f"-{target}.gz")}


class KeepSet(unittest.TestCase):
    # Two supported apps: 0.1.9 carries pg 0.1.8, 0.1.8 carries pg 0.1.5.
    TAGS = [("0.1.9", {"pg": "0.1.8+p1.e3"}), ("0.1.8", {"pg": "0.1.5+p1.e3"})]

    def setUp(self):
        self.indexes = {
            T: index(T, {
                "pg": {
                    "0.1.4+p1.e3": {},  # older than every catalog
                    "0.1.5+p1.e3": {},
                    "0.1.6+p1.e3": {"min_app": "0.1.8"},  # what 0.1.8 switches to (c)
                    "0.1.7+p1.e3": {"min_app": "0.1.8", "yanked": "se cuelga"},
                    "0.1.8+p1.e3": {"min_app": "0.1.9"},
                    "0.1.10+p1.e3": {"min_app": "0.1.9"},  # newest (b), what 0.1.9 switches to (c)
                    "0.1.11+p1.e3": {"min_app": "0.1.9", "yanked": "mal"},
                    "0.2.0+p1.e2": {},  # newest of an older epoch (b)
                    "0.1.9+p1.e2": {},
                },
            }),
        }

    def test_union(self):
        keep = names(prune.keep_set(self.TAGS, self.indexes))
        f = lambda i: prune.file_name("pg", i, T)
        self.assertEqual(keep, {f("0.1.8+p1.e3"), f("0.1.5+p1.e3"), f("0.1.10+p1.e3"), f("0.1.6+p1.e3"), f("0.2.0+p1.e2")})

    def test_a_covers_every_target(self):
        keep = prune.keep_set(self.TAGS, {})
        self.assertEqual(len(keep), 2 * len(prune.TARGETS))

    def test_yanked_in_a_catalog_stays(self):
        self.indexes[T]["drivers"]["pg"]["0.1.8+p1.e3"]["yanked"] = "mal"
        keep = names(prune.keep_set(self.TAGS, self.indexes))
        self.assertIn(prune.file_name("pg", "0.1.8+p1.e3", T), keep)

    def test_drop_with_grace(self):
        keep = prune.keep_set(self.TAGS, self.indexes)
        old = "2026-09-01T00:00:00Z"
        assets = [{"name": n, "created_at": old} for n in (x["file"] for x in self.indexes[T]["drivers"]["pg"].values())]
        assets += [
            {"name": f"index-{T}.json", "created_at": old},
            # Uploaded an hour ago and not in the index yet: a driver release in flight.
            {"name": prune.file_name("pg", "0.1.12+p1.e3", T), "created_at": "2026-10-09T11:00:00Z"},
            # Not in the index and old: an abandoned upload.
            {"name": prune.file_name("pg", "0.1.3+p1.e3", T), "created_at": old},
        ]
        drop = {a["name"] for a in prune.to_drop(assets, keep, self.indexes, NOW)}
        f = lambda i: prune.file_name("pg", i, T)
        self.assertEqual(drop, {f("0.1.4+p1.e3"), f("0.1.7+p1.e3"), f("0.1.11+p1.e3"), f("0.1.9+p1.e2"), f("0.1.3+p1.e3")})


class FailSafe(unittest.TestCase):
    def test_newest_release_missing_files(self):
        hosts = {"a.gz", "b.gz"}
        self.assertIn("faltan 1 de 2", prune.check_safe("v0.1.9", hosts, {"a.gz"}, {t: {} for t in prune.TARGETS}))
        self.assertIn("faltan 0 de 0", prune.check_safe("v0.1.9", set(), {"a.gz"}, {t: {} for t in prune.TARGETS}))

    def test_missing_index(self):
        indexes = {t: {} for t in prune.TARGETS[1:]}
        self.assertIn(prune.TARGETS[0], prune.check_safe("v0.1.9", {"a.gz"}, {"a.gz"}, indexes))

    def test_all_there(self):
        self.assertIsNone(prune.check_safe("v0.1.9", {"a.gz"}, {"a.gz", "b.gz"}, {t: {} for t in prune.TARGETS}))


class FromGit(unittest.TestCase):
    """The catalog of a real app tag, rebuilt from git (read only)."""

    def test_floors_of_a_release(self):
        tags = subprocess.run(["git", "tag", "--list", "v0.1.9"], cwd=HERE, capture_output=True, text=True).stdout.split()
        if not tags:
            self.skipTest("sin el tag v0.1.9")
        floors = prune.floors_of("v0.1.9")
        self.assertIn("postgres", floors)
        self.assertNotIn("sqlite", floors)  # built into the app
        for i in floors.values():
            self.assertRegex(i, r"^\d+\.\d+\.\d+\+p\d+\.e\d+$")


if __name__ == "__main__":
    unittest.main()
