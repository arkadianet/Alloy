#!/usr/bin/env python3
"""Post-run oracle for the live RFC-0016 holdout sweep.

This file never contacts the model endpoint. It validates the final workspace
after the real alloy process has finished.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any


TIMEOUT_EXIT_CODE = 124
UNEXECUTABLE_EXIT_CODES = {126, 127}


def target_path(manifest_path: Path) -> str:
    import tomllib

    with manifest_path.open("rb") as handle:
        manifest = tomllib.load(handle)
    return manifest["naive_target_path"]


def classify_process(
    exit_code: int,
    log: str,
    compile_clean: bool,
    reference_match: bool,
) -> str:
    if exit_code == 0 and compile_clean and reference_match:
        return "pass"
    if exit_code == 0 and not compile_clean:
        return "process_claimed_success_but_compile_failed"
    if exit_code == 0 and not reference_match:
        return "reference_mismatch"
    if exit_code == TIMEOUT_EXIT_CODE:
        return "timeout"
    if exit_code in UNEXECUTABLE_EXIT_CODES:
        return "harness_error"
    if 'reason="kind"' in log:
        return "replan_declined_kind"
    if 'reason="exhausted"' in log:
        return "repair_budget_exhausted"
    if 'reason="deadline"' in log:
        return "repair_deadline"
    if "repair generation replanned" in log:
        return "process_failed_after_replan"
    return "process_failed"


def inspect(
    fixture_dir: Path,
    workspace: Path,
    run_log: Path,
    exit_code: int,
    cargo_timeout: int,
) -> dict[str, Any]:
    relative_target = target_path(fixture_dir / "manifest.toml")
    actual = workspace / relative_target
    expected = fixture_dir / "workspace" / f"{relative_target}.post"
    log = run_log.read_text(encoding="utf-8", errors="replace")

    reference_match = (
        actual.is_file()
        and expected.is_file()
        and actual.read_bytes() == expected.read_bytes()
    )
    cargo_exit: int | None = None
    compile_clean = False
    cargo_log = workspace / "oracle-cargo.log"
    if (
        actual.is_file()
        and exit_code not in UNEXECUTABLE_EXIT_CODES
        and exit_code != TIMEOUT_EXIT_CODE
    ):
        try:
            with cargo_log.open("w", encoding="utf-8") as handle:
                result = subprocess.run(
                    ["cargo", "check", "--offline", "--quiet"],
                    cwd=workspace,
                    stdout=handle,
                    stderr=subprocess.STDOUT,
                    timeout=cargo_timeout,
                    check=False,
                )
            cargo_exit = result.returncode
            compile_clean = result.returncode == 0
        except subprocess.TimeoutExpired:
            cargo_exit = TIMEOUT_EXIT_CODE
    failure_class = classify_process(exit_code, log, compile_clean, reference_match)
    return {
        "process_pass": exit_code == 0,
        "compile_clean": compile_clean,
        "reference_match": reference_match,
        "oracle_pass": failure_class == "pass",
        "failure_class": failure_class,
        "cargo_check_exit": cargo_exit,
        "repair_generations": len(re.findall(r"repair generation replanned", log)),
        "oracle": "strict_reference",
    }


def self_test() -> None:
    assert classify_process(0, "", True, True) == "pass"
    assert classify_process(0, "", True, False) == "reference_mismatch"
    assert classify_process(5, 'reason="kind"', False, False) == "replan_declined_kind"
    assert classify_process(5, 'reason="exhausted"', False, False) == "repair_budget_exhausted"
    assert classify_process(TIMEOUT_EXIT_CODE, "", False, False) == "timeout"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture-dir", type=Path)
    parser.add_argument("--workspace", type=Path)
    parser.add_argument("--run-log", type=Path)
    parser.add_argument("--exit-code", type=int)
    parser.add_argument("--cargo-timeout", type=int, default=600)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return 0
    required = (args.fixture_dir, args.workspace, args.run_log, args.exit_code)
    if any(value is None for value in required):
        parser.error(
            "fixture execution requires --fixture-dir, --workspace, --run-log, and --exit-code"
        )
    print(
        json.dumps(
            inspect(
                args.fixture_dir,
                args.workspace,
                args.run_log,
                args.exit_code,
                args.cargo_timeout,
            ),
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
