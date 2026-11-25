#!/usr/bin/env python3
"""Tests of usage-report.py: python3 scripts/test_usage_report.py"""

import datetime as dt
import importlib.util
import pathlib
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("usage", HERE / "usage-report.py")
usage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(usage)

UTC = dt.timezone.utc


def release(tag, published, checks=0, dmg=0, draft=False):
    assets = [{"name": "latest.json", "download_count": checks}, {"name": f"DBine_{tag}_aarch64.dmg", "download_count": dmg},
              {"name": "DBine_x64-setup.exe", "download_count": 999}]
    return {"tag_name": tag, "published_at": published, "draft": draft, "assets": assets}


class Windows(unittest.TestCase):
    def test_each_release_counts_while_it_is_the_newest(self):
        rows = usage.windows([
            release("v1.1.0", "2026-11-03T00:00:00Z", checks=200, dmg=40),
            release("v1.0.0", "2026-11-01T00:00:00Z", checks=20, dmg=10),
            release("drivers", "2026-10-01T00:00:00Z", checks=5000),
            release("v1.2.0", "2026-11-04T00:00:00Z", checks=999, draft=True),
        ], dt.datetime(2026, 11, 7, tzinfo=UTC))
        self.assertEqual([r["tag"] for r in rows], ["v1.0.0", "v1.1.0"], "no drivers release, no drafts, oldest first")
        self.assertAlmostEqual(rows[0]["days"], 2)
        self.assertAlmostEqual(rows[0]["checks_per_day"], 10)
        self.assertAlmostEqual(rows[0]["mac_new_per_day"], 5, msg="only the .dmg, never the .exe updates download")
        self.assertAlmostEqual(rows[1]["days"], 4, msg="the newest counts until now")
        self.assertAlmostEqual(rows[1]["checks_per_day"], 50)

    def test_a_period_counts_for_its_part_after_the_start(self):
        rows = usage.windows([
            release("v1.0.0", "2026-11-01T00:00:00Z", checks=100),
            release("v1.1.0", "2026-11-11T00:00:00Z", checks=200),
        ], dt.datetime(2026, 11, 21, tzinfo=UTC))
        # The last 15 days: 5 days at 10/day and 10 days at 20/day.
        self.assertAlmostEqual(usage.per_day_since(rows, dt.datetime(2026, 11, 6, tzinfo=UTC)), (50 + 200) / 15)
        self.assertEqual(usage.per_day_since(rows, dt.datetime(2026, 12, 1, tzinfo=UTC)), 0.0)

    def test_a_release_published_a_moment_ago_does_not_divide_by_zero(self):
        rows = usage.windows([release("v1.0.0", "2026-11-01T00:00:00Z", checks=3)], dt.datetime(2026, 11, 1, tzinfo=UTC))
        self.assertGreater(rows[0]["days"], 0)


if __name__ == "__main__":
    unittest.main()
