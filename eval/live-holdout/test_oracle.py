#!/usr/bin/env python3
"""Tests for the live holdout's post-run correctness oracle."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from oracle import inspect


class OracleTests(unittest.TestCase):
    def make_fixture(self, root: Path) -> tuple[Path, Path, Path]:
        fixture = root / "fixture"
        workspace = root / "workspace"
        (fixture / "workspace").mkdir(parents=True)
        (workspace / "src").mkdir(parents=True)
        (fixture / "manifest.toml").write_text(
            'manifest_version = 1\nnaive_target_path = "src/lib.rs"\n',
            encoding="utf-8",
        )
        (fixture / "workspace/src/lib.rs.post").parent.mkdir(
            parents=True,
            exist_ok=True,
        )
        (fixture / "workspace/src/lib.rs.post").write_text(
            "pub fn value() -> i32 { 1 }\n",
            encoding="utf-8",
        )
        (workspace / "Cargo.toml").write_text(
            "[package]\nname = \"oracle-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
            "[lib]\npath = \"src/lib.rs\"\n",
            encoding="utf-8",
        )
        target = workspace / "src/lib.rs"
        target.write_text("pub fn value() -> i32 { 1 }\n", encoding="utf-8")
        log = root / "run.log"
        log.write_text("run finished dag_state=Succeeded\n", encoding="utf-8")
        return fixture, workspace, log

    def test_compile_clean_reference_match_is_strict_pass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, workspace, log = self.make_fixture(Path(directory))
            result = inspect(fixture, workspace, log, 0, 30)
            self.assertTrue(result["compile_clean"])
            self.assertTrue(result["reference_match"])
            self.assertTrue(result["oracle_pass"])

            (workspace / "src/lib.rs").write_text(
                "pub fn value() -> i32 { 2 }\n",
                encoding="utf-8",
            )
            result = inspect(fixture, workspace, log, 0, 30)
            self.assertTrue(result["compile_clean"])
            self.assertFalse(result["reference_match"])
            self.assertEqual(result["failure_class"], "reference_mismatch")
            self.assertFalse(result["oracle_pass"])

    def test_replan_decline_is_classified_without_a_compile_probe(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, workspace, log = self.make_fixture(Path(directory))
            (workspace / "src/lib.rs").unlink()
            log.write_text('repair generation declined reason="kind"\n', encoding="utf-8")
            result = inspect(fixture, workspace, log, 5, 30)
            self.assertEqual(result["failure_class"], "replan_declined_kind")
            self.assertFalse(result["oracle_pass"])


if __name__ == "__main__":
    unittest.main()
