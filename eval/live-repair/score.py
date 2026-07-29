#!/usr/bin/env python3
"""Score live-repair benchmark JSONL: per-fixture and overall pass rates
with Wilson 95% intervals. Author: arkadianet.

Usage: score.py results.jsonl [more.jsonl ...]
"""
import json
import math
import sys
from collections import defaultdict


def wilson(passed: int, n: int, z: float = 1.96) -> tuple[float, float]:
    if n == 0:
        return (0.0, 0.0)
    p = passed / n
    denom = 1 + z * z / n
    centre = p + z * z / (2 * n)
    margin = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n))
    return ((centre - margin) / denom, (centre + margin) / denom)


def main() -> None:
    rows = []
    for path in sys.argv[1:]:
        with open(path, encoding="utf-8") as fh:
            rows.extend(json.loads(line) for line in fh if line.strip())
    by_config = defaultdict(list)
    for r in rows:
        by_config[(r["model"], r["temp"])].append(r)
    for (model, temp), config_rows in sorted(by_config.items()):
        print(f"\n== {model} @ temperature {temp} ==")
        by_fixture = defaultdict(list)
        for r in config_rows:
            by_fixture[r["fixture"]].append(r)
        for fixture, fr in sorted(by_fixture.items()):
            n = len(fr)
            passed = sum(1 for r in fr if r["exit"] == 0)
            retried = sum(r.get("retries", 0) for r in fr)
            secs = sum(r["secs"] for r in fr) / n
            lo, hi = wilson(passed, n)
            print(
                f"  {fixture:24s} {passed:2d}/{n:<3d} retries={retried:<3d} "
                f"avg={secs:5.1f}s (95% CI {lo:.0%}–{hi:.0%})"
            )
        n = len(config_rows)
        passed = sum(1 for r in config_rows if r["exit"] == 0)
        lo, hi = wilson(passed, n)
        retried_runs = sum(1 for r in config_rows if r["exit"] == 0 and r.get("retries", 0) > 0)
        print(
            f"  {'OVERALL':24s} {passed}/{n} = {passed / n:.0%}"
            f"  (95% CI {lo:.0%}–{hi:.0%}); {retried_runs} passes via retry"
        )


if __name__ == "__main__":
    main()
