#!/usr/bin/env python3
"""Compare paired pipeline profiles; fail closed unless the frozen 15% contract passes."""
import argparse
import json
import math
import pathlib
import re
import statistics

WORKLOADS = ("transfer-legacy", "transfer-azul", "storage-legacy", "storage-azul")
ACTIVE = "base_builder_active_block_build_duration"
WALL = "base_builder_block_build_wall_duration"
# Rounded small-sample Student-t critical values; add 0.001 below to round conservatively upward.
T95 = {1: 12.706, 2: 4.303, 3: 3.182, 4: 2.776, 5: 2.571, 6: 2.447, 7: 2.365,
       8: 2.306, 9: 2.262, 10: 2.228, 11: 2.201, 12: 2.179, 13: 2.160, 14: 2.145,
       15: 2.131, 16: 2.120, 17: 2.110, 18: 2.101, 19: 2.093, 20: 2.086,
       24: 2.064, 29: 2.045, 39: 2.023, 59: 2.001, 119: 1.980}


def percentile(values, p):
    values = sorted(values)
    return values[max(0, math.ceil(len(values) * p) - 1)]


def summarize(samples, log):
    if any(error in log for error in ("Persistence service failed", "Termination failed", "panicked at", "critical task exited")):
        raise ValueError("node background task or persistence failure in benchmark log")
    active = [sample["timings"][ACTIVE][0] * 1000 for sample in samples]
    wall = [sample["timings"][WALL][0] * 1000 for sample in samples]
    driver = [sample["driver_wall_seconds"] * 1000 for sample in samples]
    rss = re.search(r"(\d+)\s+maximum resident set size", log)
    stages = {}
    for name in samples[0]["timings"]:
        stages[name] = statistics.mean(sum(sample["timings"].get(name, [])) * 1000 for sample in samples)
    return {"mean_ms": statistics.mean(active), "p50_ms": percentile(active, .5),
            "p95_ms": percentile(active, .95), "wall_p95_ms": percentile(wall, .95),
            "driver_p95_ms": percentile(driver, .95), "peak_rss_bytes": int(rss[1]) if rss else None,
            "stages_ms": stages}


def compare(baseline, candidate, coverage=False):
    names = sorted(path.name for path in baseline.glob("[0-9][0-9]-*.json"))
    if len(names) < 20 or names != sorted(path.name for path in candidate.glob("[0-9][0-9]-*.json")):
        raise ValueError("at least five complete matching four-workload run pairs required")
    runs = []
    pairs = {}
    failures = []
    load_classes = set()
    for name in names:
        before = json.loads((baseline / name).read_text())
        after = json.loads((candidate / name).read_text())
        if len(before) != 10 or len(after) != 10:
            raise ValueError("exactly ten measured blocks required per workload/run")
        for left, right in zip(before, after):
            load_class = left.get("load_class", "reference")
            load_classes.add(load_class)
            if load_class != right.get("load_class", "reference") or load_class not in ("reference", "normal", "stress"):
                raise ValueError("load-class mismatch")
            expected_transactions = 1025 if load_class == "reference" else (161 if load_class == "normal" else (2049 if "storage" in name else 2401))
            expected_publications = 11 if load_class == "normal" else 6
            wall_budget_seconds = 2.5 if load_class == "normal" else 1.5
            if left["transactions"] != expected_transactions or right["transactions"] != expected_transactions or left["gas_used"] != right["gas_used"]:
                raise ValueError(f"transaction/gas work changed in {name}")
            for sample in (left, right):
                if len(sample["timings"][ACTIVE]) != 1 or len(sample["timings"][WALL]) != 1:
                    raise ValueError("each block must have exactly one active/wall observation")
                if sample.get("published_flashblocks") != expected_publications or sample.get("observations", {}).get(ACTIVE) != 1:
                    raise ValueError("missing flashblocks or noncanonical observation count")
                if sample["observations"].get(WALL) != 1:
                    raise ValueError("expected one complete wall-clock observation")
                durations = [sample["timings"][metric][0] for metric in (ACTIVE, WALL)]
                if not all(math.isfinite(value) and value > 0 for value in durations) or durations[0] > durations[1]:
                    raise ValueError("invalid pipeline duration")
                deadlines = sample.get("deadlines")
                if not isinstance(deadlines, dict):
                    raise ValueError("missing explicit deadline observations")
                if any(deadlines.get(metric) != 0 for metric in ("payload_job_expirations", "missing_flashblocks", "reduced_flashblocks")):
                    raise ValueError("production deadline or flashblock miss")
                if deadlines.get("build_wall_budget_seconds") != wall_budget_seconds or deadlines.get("build_wall_budget_missed") is not False or durations[1] > wall_budget_seconds:
                    raise ValueError("build wall deadline missed or inconsistent evidence")
                if load_class == "stress":
                    resource_used = sample["gas_used"] if "storage" in name else sample.get("da_footprint_gas", 0)
                    if not 80_000_000 <= resource_used <= 100_000_000:
                        raise ValueError("stress workload did not exercise near-capacity gas/DA budgets")
                if load_class == "normal" and "storage" in name and not 20_000_000 <= sample["gas_used"] <= 60_200_000:
                    raise ValueError("normal storage workload outside sampled production gas regime")
        b = summarize(before, (baseline / name.replace(".json", ".log")).read_text())
        o = summarize(after, (candidate / name.replace(".json", ".log")).read_text())
        runs.append({"name": name, "baseline": b, "candidate": o, "reduction_percent": 100 * (1 - o["mean_ms"] / b["mean_ms"])})
        pair = pairs.setdefault(name[:2], {"baseline": [], "candidate": []})
        pair["baseline"].append(b["mean_ms"])
        pair["candidate"].append(o["mean_ms"])
    if len(load_classes) != 1 or (coverage and "reference" in load_classes) or (not coverage and load_classes != {"reference"}):
        raise ValueError("coverage guards cannot substitute for the frozen primary reference metric")
    # Per-workload guardrails aggregate across repetitions, rather than treating noisy individual runs as conclusive.
    for workload in WORKLOADS:
        rows = [run for run in runs if run["name"].endswith(workload + ".json")]
        for guardrail in ("p95_ms", "wall_p95_ms", "driver_p95_ms", "peak_rss_bytes"):
            values_b = [row["baseline"][guardrail] for row in rows]
            values_o = [row["candidate"][guardrail] for row in rows]
            if any(value is None for value in values_b + values_o):
                failures.append(f"missing {guardrail} for {workload}")
            elif statistics.mean(values_o) > 1.05 * statistics.mean(values_b):
                failures.append(f"{guardrail} regression exceeds 5% for {workload}")
    reductions = []
    for pair in pairs.values():
        if len(pair["baseline"]) != 4:
            raise ValueError("incomplete workload pair")
        reductions.append(100 * (1 - statistics.mean(pair["candidate"]) / statistics.mean(pair["baseline"])))
    df = len(reductions) - 1
    critical = next((T95[k] for k in sorted(T95, reverse=True) if k <= df), 12.706) + 0.001
    uncertainty = critical * statistics.stdev(reductions) / math.sqrt(len(reductions))
    center = statistics.mean(reductions)
    baseline_ms = statistics.mean(run["baseline"]["mean_ms"] for run in runs)
    candidate_ms = statistics.mean(run["candidate"]["mean_ms"] for run in runs)
    if not coverage and (candidate_ms > .85 * baseline_ms or center - uncertainty < 15):
        failures.append("15% improvement not established at 95% confidence")
    return {"baseline_ms": baseline_ms, "optimized_ms": candidate_ms,
            "reduction_percent": 100 * (1 - candidate_ms / baseline_ms),
            "paired_reduction_mean_percent": center, "paired_95_percent_ci": [center - uncertainty, center + uncertainty],
            "independent_pairs": len(pairs), "load_class": next(iter(load_classes)),
            "performance_target_required": not coverage, "deadline_misses": 0,
            "failures": failures, "passed": not failures, "runs": runs}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=pathlib.Path)
    parser.add_argument("candidate", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--coverage", action="store_true", help="Supplemental normal/stress safety checks; cannot certify the reference performance target")
    args = parser.parse_args()
    result = compare(args.baseline, args.candidate, coverage=args.coverage)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({k: v for k, v in result.items() if k != "runs"}, indent=2))
    raise SystemExit(0 if result["passed"] else 1)


if __name__ == "__main__":
    main()
