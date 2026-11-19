#!/usr/bin/env python3
"""Tests of downloads-badge.py: python3 scripts/test_downloads_badge.py"""

import importlib.util
import pathlib
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("badge", HERE / "downloads-badge.py")
badge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(badge)


def release(tag, assets, draft=False):
    return {"tag_name": tag, "draft": draft, "assets": [{"name": n, "download_count": c} for n, c in assets.items()]}


class Count(unittest.TestCase):
    def test_every_installer_and_update_of_the_app_counts(self):
        r = release("v1.6.3", {
            "DBine_1.6.3_x64-setup.exe": 10,
            "DBine_1.6.3_x64_en-US.msi": 1,
            "DBine_1.6.3_aarch64.dmg": 5,
            "DBine_1.6.3_x64.dmg": 2,
            "DBine_1.6.3_amd64.AppImage": 3,
            "DBine_1.6.3_amd64.deb": 4,
            "DBine-1.6.3-1.x86_64.rpm": 1,
            "dbine-1.6.3-x86_64.flatpak": 2,
            "DBine-1.6.3-android.apk": 1,
            # macOS updates.
            "DBine_aarch64.app.tar.gz": 7,
            "DBine_x64.app.tar.gz": 1,
        })
        self.assertEqual(badge.total([r]), 37)

    def test_drivers_update_checks_signatures_and_provenance_dont(self):
        app = release("v1.6.3", {
            "DBine_1.6.3_x64-setup.exe": 10,
            "latest.json": 500,
            "DBine_1.6.3_x64-setup.exe.sig": 40,
            "DBine_aarch64.app.tar.gz.sig": 30,
            "dbine-v1.6.3.intoto.jsonl": 2,
        })
        drivers = release("drivers", {
            "dbine-driver-sqlserver-0.1.22+p1.e4-x86_64-pc-windows-msvc.gz": 900,
            "index-x86_64-pc-windows-msvc.json": 800,
        })
        self.assertEqual(badge.total([app, drivers]), 10)

    def test_a_draft_release_doesnt_count(self):
        self.assertEqual(badge.total([release("v1.7.0", {"DBine_1.7.0_aarch64.dmg": 3}, draft=True)]), 0)

    def test_every_release_adds_up(self):
        rs = [release("v1.6.2", {"DBine_1.6.2_aarch64.dmg": 4}), release("v1.6.3", {"DBine_1.6.3_aarch64.dmg": 6})]
        self.assertEqual(badge.total(rs), 10)


class Format(unittest.TestCase):
    def test_like_shields_download_counts(self):
        cases = {0: "0", 950: "950", 999: "999", 1000: "1.0k", 3983: "4.0k", 7185: "7.2k",
                 12_345: "12k", 999_499: "999k", 999_600: "1.0M", 1_500_000: "1.5M"}
        for n, text in cases.items():
            self.assertEqual(badge.metric(n), text, n)

    def test_the_badge_is_a_shields_endpoint(self):
        b = badge.badge(3983)
        self.assertEqual(b["schemaVersion"], 1)
        self.assertEqual((b["label"], b["message"]), ("downloads", "4.0k"))


if __name__ == "__main__":
    unittest.main()
