#!/usr/bin/env python3
"""Run independent live-holdout arms and compare their reports."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

from compare import compare_reports, load_report, render_summary


ARM_ID = re.compile(r"^[A-Za-z0-9_.-]+$")


@dataclass(frozen=True)
class Arm:
    arm_id: str
    model: str
    temperature: str
    profile: str
    base_url: str
    reps: int


def parse_arms(path: Path) -> list[Arm]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise ValueError(f"{path}: cannot read arms file: {error}") from error
    arms: list[Arm] = []
    seen: set[str] = set()
    for line_number, line in enumerate(lines, start=1):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        fields = line.split("\t")
        if fields == ["arm_id", "model", "temperature", "profile", "base_url", "reps"]:
            continue
        if len(fields) != 6:
            raise ValueError(
                f"{path}:{line_number}: expected six tab-separated fields "
                "ARM_ID MODEL TEMPERATURE PROFILE BASE_URL REPS"
            )
        arm_id, model, temperature, profile, base_url, reps_text = (
            field.strip() for field in fields
        )
        if not ARM_ID.fullmatch(arm_id):
            raise ValueError(f"{path}:{line_number}: invalid arm id {arm_id!r}")
        if arm_id in seen:
            raise ValueError(f"{path}:{line_number}: duplicate arm id {arm_id!r}")
        if not model or not base_url:
            raise ValueError(f"{path}:{line_number}: model and base_url are required")
        if profile not in {"default", "autonomous"}:
            raise ValueError(f"{path}:{line_number}: profile must be default or autonomous")
        try:
            reps = int(reps_text)
        except ValueError as error:
            raise ValueError(f"{path}:{line_number}: reps must be an integer") from error
        if reps < 1:
            raise ValueError(f"{path}:{line_number}: reps must be at least 1")
        arms.append(Arm(arm_id, model, temperature, profile, base_url, reps))
        seen.add(arm_id)
    if len(arms) < 2:
        raise ValueError(f"{path}: at least two arms are required")
    return arms


def run_arm(
    arm: Arm,
    run_script: Path,
    output_dir: Path,
    inherited_env: dict[str, str],
) -> tuple[Path, int]:
    observations = output_dir / f"{arm.arm_id}.jsonl"
    env = inherited_env | {
        "MODEL": arm.model,
        "TEMP": arm.temperature,
        "PROFILE": arm.profile,
        "BASEURL": arm.base_url,
        "REPS": str(arm.reps),
        "SCORE": "1",
    }
    print(
        f"RUN {arm.arm_id}: model={arm.model} profile={arm.profile} "
        f"temp={arm.temperature} reps={arm.reps}"
    )
    completed = subprocess.run(["bash", str(run_script), str(observations)], env=env)
    return observations.with_suffix(".report.json"), completed.returncode


def parse_args(argv: Iterable[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--arms", type=Path, required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument(
        "--run-script",
        type=Path,
        default=Path(__file__).with_name("run.sh"),
    )
    parser.add_argument("--comparison-out", type=Path)
    return parser.parse_args(list(argv))


def main(argv: Iterable[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        args.arms = args.arms.resolve()
        args.out_dir = args.out_dir.resolve()
        args.run_script = args.run_script.resolve()
        if args.comparison_out is not None:
            args.comparison_out = args.comparison_out.resolve()
        arms = parse_arms(args.arms)
        if not args.run_script.is_file():
            raise ValueError(f"run script missing: {args.run_script}")
        args.out_dir.mkdir(parents=True, exist_ok=True)
        reports: list[tuple[str, dict[str, object]]] = []
        failures: list[str] = []
        for arm in arms:
            report_path, status = run_arm(arm, args.run_script, args.out_dir, dict(os.environ))
            if status != 0:
                failures.append(f"{arm.arm_id} exited {status}")
            if report_path.is_file():
                reports.append((arm.arm_id, load_report(report_path)))
            else:
                failures.append(f"{arm.arm_id} did not produce {report_path}")
        if failures:
            raise ValueError("matrix arm failures: " + "; ".join(failures))
        comparison = compare_reports(reports)
        comparison_out = args.comparison_out or args.out_dir / "matrix.report.json"
        comparison_out.write_text(json.dumps(comparison, indent=2) + "\n", encoding="utf-8")
        print(render_summary(comparison))
        print(f"comparison_report={comparison_out}")
        return 0
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"live-holdout/matrix.py: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
