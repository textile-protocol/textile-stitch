#!/usr/bin/env python3
"""Tests for Actions artifact cleanup + retention-days workflow lint."""

from __future__ import annotations

import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

from cleanup_actions_artifacts import (
    default_workflow_dirs,
    is_old_enough,
    lint_workflows,
    missing_retention_days,
    select_deletable,
)


NOW = datetime(2026, 8, 14, 12, 0, tzinfo=timezone.utc)
SIX_HOURS = timedelta(hours=6)


class SelectDeletableTest(unittest.TestCase):
    def test_keeps_recent_artifacts(self) -> None:
        artifacts = [
            {
                "id": 1,
                "name": "fresh",
                "created_at": "2026-08-14T10:00:00Z",
                "expired": False,
                "size_in_bytes": 10,
            },
            {
                "id": 2,
                "name": "old",
                "created_at": "2026-08-13T12:00:00Z",
                "expired": False,
                "size_in_bytes": 20,
            },
            {
                "id": 3,
                "name": "expired-old",
                "created_at": "2026-07-01T00:00:00Z",
                "expired": True,
                "size_in_bytes": 30,
            },
        ]
        deletable = select_deletable(artifacts, NOW, SIX_HOURS)
        self.assertEqual([item["name"] for item in deletable], ["old"])

    def test_boundary_is_inclusive(self) -> None:
        self.assertTrue(is_old_enough("2026-08-14T06:00:00Z", NOW, SIX_HOURS))
        self.assertFalse(is_old_enough("2026-08-14T06:00:01Z", NOW, SIX_HOURS))


class MissingRetentionDaysTest(unittest.TestCase):
    def test_reports_upload_without_retention(self) -> None:
        text = """
      - name: Upload source export
        uses: actions/upload-artifact@v4
        with:
          name: payload
          path: payload.tar.gz

      - name: Next step
        run: echo hi
"""
        self.assertEqual(missing_retention_days(text), [3])

    def test_accepts_retention_days(self) -> None:
        text = """
      - name: Upload source export
        uses: actions/upload-artifact@v4
        with:
          name: payload
          path: payload.tar.gz
          retention-days: 1
"""
        self.assertEqual(missing_retention_days(text), [])

    def test_ignores_download_artifact(self) -> None:
        text = """
      - uses: actions/download-artifact@v4
        with:
          name: payload
"""
        self.assertEqual(missing_retention_days(text), [])


class WorkflowLintTest(unittest.TestCase):
    def test_checked_in_workflows_set_retention(self) -> None:
        repo_root = Path(__file__).resolve().parents[2]
        failures = lint_workflows(default_workflow_dirs(repo_root))
        self.assertEqual(failures, 0)


if __name__ == "__main__":
    unittest.main()
