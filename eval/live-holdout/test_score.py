#!/usr/bin/env python3
"""Tests for strict live-holdout report validation and aggregation."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from score import summarize, validate_rows, wilson_interval


MODEL = "stub-model"
BASE_URL = "http://127.0.0.1:8089/v1/"


def row(fixture_id: str, repetition: int, oracle_pass: bool) -> dict[str, object]:
    return {
        "fixture_id": fixture_id,
        "repetition": repetition,
        "exit_code": 0 if oracle_pass else 1,
        "process_pass": oracle_pass,
        "compile_clean": oracle_pass,
        "reference_match": oracle_pass,
        "oracle_pass": oracle_pass,
        "failure_class": "pass" if oracle_pass else "process_failed",
        "cargo_check_exit": 0 if oracle_pass else None,
        "repair_generations": 1,
        "wall_ms": 10,
        "model": MODEL,
        "temperature": 0.6,
        "base_url": BASE_URL,
        "corpus": "rfc0016-holdout-live",
    }


class ScoreTests(unittest.TestCase):
    def fixture_root(self) -> tempfile.TemporaryDirectory[str]:
        directory = tempfile.TemporaryDirectory()
        fixture = Path(directory.name) / "fixture_a"
        fixture.mkdir()
        (fixture / "manifest.toml").write_text("manifest_version = 1\n", encoding="utf-8")
        return directory

    def test_validates_dense_repetitions_and_reports_wilson_interval(self) -> None:
        with self.fixture_root() as directory:
            root = Path(directory)
            rows = [row("fixture_a", 1, True), row("fixture_a", 2, False)]
            grouped = validate_rows(rows, ["fixture_a"], MODEL, 0.6, BASE_URL, 2)
            report = summarize(grouped, MODEL, 0.6, BASE_URL, 2)
            self.assertEqual(report["overall"]["oracle"]["passes"], 1)
            self.assertEqual(report["overall"]["oracle"]["attempts"], 2)
            self.assertEqual(
                report["overall"]["oracle"]["wilson95"],
                wilson_interval(1, 2),
            )
            self.assertEqual(report["overall"]["failure_classes"]["process_failed"], 1)
            self.assertTrue((root / "fixture_a/manifest.toml").is_file())

    def test_rejects_missing_repetition_and_endpoint_mismatch(self) -> None:
        with self.fixture_root():
            with self.assertRaisesRegex(ValueError, "repetitions"):
                validate_rows(
                    [row("fixture_a", 2, True)],
                    ["fixture_a"],
                    MODEL,
                    0.6,
                    BASE_URL,
                    2,
                )

            mismatched = row("fixture_a", 1, True)
            mismatched["model"] = "other-model"
            with self.assertRaisesRegex(ValueError, "endpoint mismatch"):
                validate_rows(
                    [mismatched],
                    ["fixture_a"],
                    MODEL,
                    0.6,
                    BASE_URL,
                    1,
                )

    def test_rejects_inconsistent_compile_and_failure_fields(self) -> None:
        with self.fixture_root():
            inconsistent_compile = row("fixture_a", 1, True)
            inconsistent_compile["cargo_check_exit"] = 101
            with self.assertRaisesRegex(ValueError, "cargo_check_exit"):
                validate_rows(
                    [inconsistent_compile],
                    ["fixture_a"],
                    MODEL,
                    0.6,
                    BASE_URL,
                    1,
                )

            inconsistent_failure = row("fixture_a", 1, False)
            inconsistent_failure["failure_class"] = "pass"
            with self.assertRaisesRegex(ValueError, "failure_class"):
                validate_rows(
                    [inconsistent_failure],
                    ["fixture_a"],
                    MODEL,
                    0.6,
                    BASE_URL,
                    1,
                )


if __name__ == "__main__":
    unittest.main()
