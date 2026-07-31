#!/usr/bin/env python3
"""Tests for live-holdout matrix configuration parsing."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from matrix import parse_arms


class MatrixTests(unittest.TestCase):
    def write_arms(self, content: str) -> Path:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / "arms.tsv"
        path.write_text(content, encoding="utf-8")
        return path

    def test_parses_header_comments_and_independent_repetitions(self) -> None:
        path = self.write_arms(
            "# model/context arms\n"
            "arm_id\tmodel\ttemperature\tprofile\tbase_url\treps\n"
            "baseline\tstub-a\t0.2\tdefault\thttp://a/v1/\t30\n"
            "context\tstub-a\t0.2\tautonomous\thttp://a/v1/\t30\n"
        )
        arms = parse_arms(path)
        self.assertEqual(arms[0].arm_id, "baseline")
        self.assertEqual(arms[1].profile, "autonomous")
        self.assertEqual(arms[1].reps, 30)

    def test_rejects_duplicate_arm_ids(self) -> None:
        path = self.write_arms(
            "a\tstub\t0.2\tdefault\thttp://a/v1/\t30\n"
            "a\tstub\t0.2\tdefault\thttp://a/v1/\t30\n"
        )
        with self.assertRaisesRegex(ValueError, "duplicate arm id"):
            parse_arms(path)

    def test_requires_at_least_two_arms(self) -> None:
        path = self.write_arms("a\tstub\t0.2\tdefault\thttp://a/v1/\t30\n")
        with self.assertRaisesRegex(ValueError, "at least two"):
            parse_arms(path)


if __name__ == "__main__":
    unittest.main()
