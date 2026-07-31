#!/usr/bin/env python3
"""Tests for independent live-holdout report comparison."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from compare import compare_reports


def rate(passes: int, attempts: int = 2) -> dict[str, object]:
    return {
        "passes": passes,
        "attempts": attempts,
        "rate": passes / attempts,
        "wilson95": {"low": 0.0, "high": 1.0},
    }


def fixture(fixture_id: str, oracle_passes: int) -> dict[str, object]:
    return {
        "fixture_id": fixture_id,
        "oracle": rate(oracle_passes),
        "process": rate(2),
        "compile_clean": rate(2),
        "reference_match": rate(oracle_passes),
        "failure_classes": {"pass": oracle_passes, "reference_mismatch": 2 - oracle_passes},
    }


def report(oracle_passes: int, repetitions: int = 2) -> dict[str, object]:
    fixtures = [fixture("fixture_a", oracle_passes)]
    return {
        "schema_version": 1,
        "corpus": "rfc0016-holdout-live",
        "endpoint": {
            "model": "stub",
            "temperature": 0.6,
            "profile": "default",
            "base_url": "http://example.invalid/v1/",
        },
        "repetitions": repetitions,
        "fixtures": fixtures,
        "overall": {
            "oracle": rate(oracle_passes),
            "process": rate(2),
            "compile_clean": rate(2),
            "reference_match": rate(oracle_passes),
            "failure_classes": {},
        },
    }


class CompareTests(unittest.TestCase):
    def test_keeps_arms_separate_and_reports_deltas(self) -> None:
        result = compare_reports(
            [("baseline", report(1)), ("context", report(2))]
        )
        self.assertEqual([arm["arm_id"] for arm in result["arms"]], ["baseline", "context"])
        self.assertEqual(result["comparisons"][0]["oracle"]["delta"], 0.5)
        self.assertEqual(result["comparisons"][0]["by_fixture"][0]["oracle"]["delta"], 0.5)
        self.assertEqual(result["comparisons"][0]["assessment"]["result"], "improved")
        self.assertIsNone(result["comparisons"][0]["assessment"]["why_not"])
        self.assertIn("Wilson", " ".join(result["notes"]))

    def test_rejects_incompatible_repetition_counts(self) -> None:
        with self.assertRaisesRegex(ValueError, "repetitions"):
            compare_reports([("baseline", report(1)), ("context", report(2, repetitions=3))])


if __name__ == "__main__":
    unittest.main()
