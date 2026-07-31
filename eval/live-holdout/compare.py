#!/usr/bin/env python3
"""Compare independent strict-oracle live-holdout reports."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any, Iterable


REPORT_VERSION = 1


def load_report(path: Path) -> dict[str, Any]:
    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"{path}: cannot read report: {error}") from error
    if not isinstance(report, dict) or report.get("schema_version") != REPORT_VERSION:
        raise ValueError(f"{path}: unsupported or missing report schema")
    if not isinstance(report.get("fixtures"), list) or not report["fixtures"]:
        raise ValueError(f"{path}: report has no fixtures")
    return report


def endpoint(report: dict[str, Any]) -> dict[str, Any]:
    value = report.get("endpoint")
    if not isinstance(value, dict):
        raise ValueError("report endpoint must be an object")
    return {
        "model": value.get("model"),
        "temperature": value.get("temperature"),
        "profile": value.get("profile", "default"),
        "base_url": value.get("base_url"),
    }


def fixture_map(report: dict[str, Any]) -> dict[str, dict[str, Any]]:
    result: dict[str, dict[str, Any]] = {}
    for fixture in report["fixtures"]:
        if not isinstance(fixture, dict) or not isinstance(fixture.get("fixture_id"), str):
            raise ValueError("fixture entries must contain string fixture_id values")
        fixture_id = fixture["fixture_id"]
        if fixture_id in result:
            raise ValueError(f"duplicate fixture {fixture_id!r}")
        result[fixture_id] = fixture
    return result


def metric_rate(fixture: dict[str, Any], metric: str) -> float:
    value = fixture.get(metric, {}).get("rate")
    if not isinstance(value, (int, float)):
        raise ValueError(f"fixture {fixture.get('fixture_id')!r} has no {metric} rate")
    return float(value)


def metric_delta(
    left: dict[str, Any], right: dict[str, Any], metric: str
) -> dict[str, float]:
    return {
        "baseline_rate": metric_rate(left, metric),
        "arm_rate": metric_rate(right, metric),
        "delta": metric_rate(right, metric) - metric_rate(left, metric),
    }


def compare_reports(named_reports: list[tuple[str, dict[str, Any]]]) -> dict[str, Any]:
    if len(named_reports) < 2:
        raise ValueError("at least two reports are required")
    baseline_name, baseline = named_reports[0]
    baseline_fixtures = fixture_map(baseline)
    baseline_reps = baseline.get("repetitions")
    baseline_corpus = baseline.get("corpus")
    arms: list[dict[str, Any]] = []
    for name, report in named_reports:
        fixtures = fixture_map(report)
        if sorted(fixtures) != sorted(baseline_fixtures):
            raise ValueError(f"arm {name!r} does not use the same fixture set as {baseline_name!r}")
        if report.get("repetitions") != baseline_reps:
            raise ValueError(f"arm {name!r} does not use repetitions={baseline_reps}")
        if report.get("corpus") != baseline_corpus:
            raise ValueError(f"arm {name!r} does not use corpus {baseline_corpus!r}")
        overall = report.get("overall")
        if not isinstance(overall, dict):
            raise ValueError(f"arm {name!r} has no overall summary")
        arms.append(
            {
                "arm_id": name,
                "endpoint": endpoint(report),
                "repetitions": report["repetitions"],
                "overall": overall,
                "failure_classes": overall.get("failure_classes", {}),
            }
        )

    baseline_by_fixture = fixture_map(baseline)
    comparisons: list[dict[str, Any]] = []
    for name, report in named_reports[1:]:
        arm_by_fixture = fixture_map(report)
        by_fixture = []
        for fixture_id in sorted(baseline_by_fixture):
            left = baseline_by_fixture[fixture_id]
            right = arm_by_fixture[fixture_id]
            by_fixture.append(
                {
                    "fixture_id": fixture_id,
                    "oracle": metric_delta(left, right, "oracle"),
                    "process": metric_delta(left, right, "process"),
                    "compile_clean": metric_delta(left, right, "compile_clean"),
                    "reference_match": metric_delta(left, right, "reference_match"),
                    "failure_classes": right.get("failure_classes", {}),
                }
            )
        baseline_overall = baseline["overall"]
        arm_overall = report["overall"]
        oracle = metric_delta(baseline_overall, arm_overall, "oracle")
        if oracle["delta"] > 0:
            assessment = {
                "result": "improved",
                "why_not": None,
                "basis": "strict_oracle_rate_delta_positive",
            }
        elif oracle["delta"] < 0:
            assessment = {
                "result": "why_not",
                "why_not": "strict_oracle_rate_decreased",
                "basis": "strict_oracle_rate_delta_negative",
            }
        else:
            assessment = {
                "result": "why_not",
                "why_not": "no_strict_oracle_rate_change",
                "basis": "strict_oracle_rate_delta_zero",
            }
        comparisons.append(
            {
                "baseline": baseline_name,
                "arm": name,
                "oracle": oracle,
                "process": metric_delta(baseline_overall, arm_overall, "process"),
                "compile_clean": metric_delta(
                    baseline_overall, arm_overall, "compile_clean"
                ),
                "reference_match": metric_delta(
                    baseline_overall, arm_overall, "reference_match"
                ),
                "by_fixture": by_fixture,
                "assessment": assessment,
            }
        )

    return {
        "schema_version": REPORT_VERSION,
        "corpus": baseline_corpus,
        "repetitions": baseline_reps,
        "baseline": baseline_name,
        "arms": arms,
        "comparisons": comparisons,
        "notes": [
            "Each arm retains its own denominator and Wilson interval.",
            "Reports are compared only when fixture sets, corpus, and repetitions match.",
            "Deltas are descriptive; overlapping Wilson intervals are not a significance test.",
            "Strict-oracle results are live-BYOM telemetry, not an offline release gate.",
        ],
    }


def render_summary(comparison: dict[str, Any]) -> str:
    lines = [
        f"baseline={comparison['baseline']} repetitions={comparison['repetitions']}",
        "arm\toracle\tprocess\tcompile\treference",
    ]
    for arm in comparison["arms"]:
        overall = arm["overall"]
        lines.append(
            f"{arm['arm_id']}\t"
            f"{overall['oracle']['passes']}/{overall['oracle']['attempts']}\t"
            f"{overall['process']['passes']}/{overall['process']['attempts']}\t"
            f"{overall['compile_clean']['passes']}/{overall['compile_clean']['attempts']}\t"
            f"{overall['reference_match']['passes']}/{overall['reference_match']['attempts']}"
        )
    lines.append("comparison\toracle_delta\tprocess_delta\tcompile_delta\treference_delta")
    for item in comparison["comparisons"]:
        lines.append(
            f"{item['arm']}\t{item['oracle']['delta']:+.6f}\t"
            f"{item['process']['delta']:+.6f}\t"
            f"{item['compile_clean']['delta']:+.6f}\t"
            f"{item['reference_match']['delta']:+.6f}"
        )
        assessment = item["assessment"]
        lines.append(
            f"assessment\t{item['arm']}\t{assessment['result']}\t"
            f"{assessment['why_not'] or assessment['basis']}"
        )
    lines.append("NOTE: no incompatible reports were pooled.")
    return "\n".join(lines)


def parse_arm(value: str) -> tuple[str, Path]:
    name, separator, report = value.partition("=")
    if not separator or not name or not report:
        raise ValueError(f"--arm must be ARM_ID=REPORT_PATH, got {value!r}")
    if any(character not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_." for character in name):
        raise ValueError(f"invalid arm id {name!r}")
    return name, Path(report)


def parse_args(argv: Iterable[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--arm", action="append", required=True)
    parser.add_argument("--out", type=Path, required=True)
    return parser.parse_args(list(argv))


def main(argv: Iterable[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        named_reports = [(name, load_report(path)) for name, path in map(parse_arm, args.arm)]
        comparison = compare_reports(named_reports)
        args.out.write_text(json.dumps(comparison, indent=2) + "\n", encoding="utf-8")
        print(render_summary(comparison))
        return 0
    except (OSError, ValueError) as error:
        print(f"live-holdout/compare.py: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
