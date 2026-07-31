#!/usr/bin/env python3
"""Validate and summarize strict live-holdout observations."""

from __future__ import annotations

import argparse
import json
import math
import sys
from collections import Counter
from pathlib import Path
from typing import Any, Iterable


REPORT_VERSION = 1
WILSON_Z_95 = 1.96
REQUIRED_FIELDS = {
    "fixture_id",
    "repetition",
    "exit_code",
    "process_pass",
    "compile_clean",
    "reference_match",
    "oracle_pass",
    "failure_class",
    "cargo_check_exit",
    "repair_generations",
    "wall_ms",
    "model",
    "temperature",
    "base_url",
    "corpus",
}


def wilson_interval(passes: int, attempts: int) -> dict[str, float] | None:
    if attempts == 0:
        return None
    n = float(attempts)
    p = min(passes, attempts) / n
    z2 = WILSON_Z_95 * WILSON_Z_95
    denominator = 1.0 + z2 / n
    centre = p + z2 / (2.0 * n)
    margin = WILSON_Z_95 * math.sqrt(p * (1.0 - p) / n + z2 / (4.0 * n * n))
    return {
        "low": (centre - margin) / denominator,
        "high": (centre + margin) / denominator,
    }


def rate(rows: list[dict[str, Any]], field: str) -> dict[str, Any]:
    passes = sum(bool(row[field]) for row in rows)
    attempts = len(rows)
    return {
        "passes": passes,
        "attempts": attempts,
        "rate": passes / attempts if attempts else None,
        "wilson95": wilson_interval(passes, attempts),
    }


def fixture_ids(root: Path) -> list[str]:
    return sorted(
        path.name
        for path in root.iterdir()
        if path.is_dir() and (path / "manifest.toml").is_file()
    )


def validate_rows(
    rows: list[dict[str, Any]],
    expected_ids: list[str],
    model: str,
    temperature: float,
    base_url: str,
    reps: int,
) -> dict[str, list[dict[str, Any]]]:
    grouped: dict[str, list[dict[str, Any]]] = {fixture_id: [] for fixture_id in expected_ids}
    for index, row in enumerate(rows, start=1):
        missing = REQUIRED_FIELDS - row.keys()
        if missing:
            raise ValueError(f"observation line {index} missing fields: {sorted(missing)}")
        fixture_id = row["fixture_id"]
        if fixture_id not in grouped:
            raise ValueError(f"unknown fixture id {fixture_id!r} on observation line {index}")
        for field in ("process_pass", "compile_clean", "reference_match", "oracle_pass"):
            if not isinstance(row[field], bool):
                raise ValueError(f"{field} must be boolean on observation line {index}")
        if not isinstance(row["repetition"], int) or isinstance(row["repetition"], bool):
            raise ValueError(f"repetition must be an integer on observation line {index}")
        if row["process_pass"] != (row["exit_code"] == 0):
            raise ValueError(f"inconsistent process fields on observation line {index}")
        if row["model"] != model or row["base_url"] != base_url:
            raise ValueError(f"endpoint mismatch on observation line {index}")
        if not math.isclose(float(row["temperature"]), temperature, rel_tol=0.0, abs_tol=1e-12):
            raise ValueError(f"temperature mismatch on observation line {index}")
        if row["corpus"] != "rfc0016-holdout-live":
            raise ValueError(f"wrong corpus on observation line {index}")
        if row["oracle_pass"] != (
            row["process_pass"] and row["compile_clean"] and row["reference_match"]
        ):
            raise ValueError(f"inconsistent oracle fields on observation line {index}")
        grouped[fixture_id].append(row)

    for fixture_id, fixture_rows in grouped.items():
        repetitions = sorted(row["repetition"] for row in fixture_rows)
        expected = list(range(1, reps + 1))
        if repetitions != expected:
            raise ValueError(
                f"fixture {fixture_id} repetitions {repetitions!r}, expected {expected!r}"
            )
    return grouped


def summarize_fixture(fixture_id: str, rows: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "fixture_id": fixture_id,
        "process": rate(rows, "process_pass"),
        "compile_clean": rate(rows, "compile_clean"),
        "reference_match": rate(rows, "reference_match"),
        "oracle": rate(rows, "oracle_pass"),
        "failure_classes": dict(sorted(Counter(row["failure_class"] for row in rows).items())),
        "repair_generations_total": sum(row["repair_generations"] for row in rows),
        "wall_ms_total": sum(row["wall_ms"] for row in rows),
    }


def summarize(
    grouped: dict[str, list[dict[str, Any]]],
    model: str,
    temperature: float,
    base_url: str,
    reps: int,
) -> dict[str, Any]:
    all_rows = [row for rows in grouped.values() for row in rows]
    return {
        "schema_version": REPORT_VERSION,
        "corpus": "rfc0016-holdout-live",
        "endpoint": {
            "model": model,
            "temperature": temperature,
            "base_url": base_url,
        },
        "repetitions": reps,
        "fixtures": [
            summarize_fixture(fixture_id, grouped[fixture_id])
            for fixture_id in sorted(grouped)
        ],
        "overall": {
            "process": rate(all_rows, "process_pass"),
            "compile_clean": rate(all_rows, "compile_clean"),
            "reference_match": rate(all_rows, "reference_match"),
            "oracle": rate(all_rows, "oracle_pass"),
            "failure_classes": dict(
                sorted(Counter(row["failure_class"] for row in all_rows).items())
            ),
            "repair_generations_total": sum(row["repair_generations"] for row in all_rows),
            "wall_ms_total": sum(row["wall_ms"] for row in all_rows),
        },
        "observations": all_rows,
    }


def load_rows(path: Path) -> list[dict[str, Any]]:
    rows = []
    with path.open(encoding="utf-8") as handle:
        for index, line in enumerate(handle, start=1):
            if not line.strip():
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError as error:
                raise ValueError(f"invalid JSON on observation line {index}: {error}") from error
            if not isinstance(value, dict):
                raise ValueError(f"observation line {index} is not a JSON object")
            rows.append(value)
    return rows


def render_summary(report: dict[str, Any]) -> str:
    lines = ["fixture_id\toracle\tprocess\tcompile\treference\tattempts\tfailure_classes"]
    for fixture in report["fixtures"]:
        lines.append(
            f"{fixture['fixture_id']}\t"
            f"{fixture['oracle']['passes']}/{fixture['oracle']['attempts']}\t"
            f"{fixture['process']['passes']}/{fixture['process']['attempts']}\t"
            f"{fixture['compile_clean']['passes']}/{fixture['compile_clean']['attempts']}\t"
            f"{fixture['reference_match']['passes']}/{fixture['reference_match']['attempts']}\t"
            f"{fixture['oracle']['attempts']}\t"
            f"{json.dumps(fixture['failure_classes'], sort_keys=True)}"
        )
    overall = report["overall"]
    lines.append(
        f"overall\t{overall['oracle']['passes']}/{overall['oracle']['attempts']}\t"
        f"{overall['process']['passes']}/{overall['process']['attempts']}\t"
        f"{overall['compile_clean']['passes']}/{overall['compile_clean']['attempts']}\t"
        f"{overall['reference_match']['passes']}/{overall['reference_match']['attempts']}\t"
        f"{overall['oracle']['attempts']}\t"
        f"{json.dumps(overall['failure_classes'], sort_keys=True)}"
    )
    interval = overall["oracle"]["wilson95"]
    if interval is None:
        lines.append("oracle_rate=unmeasured")
    else:
        lines.append(
            f"oracle_rate={overall['oracle']['rate']:.6f} "
            f"wilson95=[{interval['low']:.6f},{interval['high']:.6f}]"
        )
    lines.append(
        "NOTE: strict live-holdout telemetry only; this is not an RFC-0016 offline gate."
    )
    return "\n".join(lines)


def parse_args(argv: Iterable[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--observations", type=Path, required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--temperature", type=float, required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--reps", type=int, required=True)
    parser.add_argument("--out", type=Path, required=True)
    return parser.parse_args(list(argv))


def main(argv: Iterable[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        if args.reps < 1:
            raise ValueError("--reps must be at least 1")
        expected_ids = fixture_ids(args.fixtures)
        if not expected_ids:
            raise ValueError(f"no fixture manifests under {args.fixtures}")
        rows = load_rows(args.observations)
        grouped = validate_rows(
            rows,
            expected_ids,
            args.model,
            args.temperature,
            args.base_url,
            args.reps,
        )
        report = summarize(grouped, args.model, args.temperature, args.base_url, args.reps)
        args.out.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(render_summary(report))
        return 0
    except (OSError, ValueError) as error:
        print(f"live-holdout/score.py: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
