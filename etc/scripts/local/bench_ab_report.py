#!/usr/bin/env python3
"""Summarize and compare `base-bench snapshot` runs: sustained gas/s and per-block time attribution.

Each argument is a `base-bench snapshot --output-dir` directory holding `benchmark-result.json`.
For every run the script prints, from the measured sequencer blocks:

- gas/s: measured gas over the measured blocks' chain time, as in the report's `gas/per_second`;
- native build stages in ms (execution, state-root wait, finish, seal) next to the whole build, and
  their difference, which should be about zero;
- engine block insert and persistence per block in ms, and whether persistence keeps up with the
  block interval (one save call writes `batch_size` blocks);
- the highest engine backpressure gauge seen;
- with a validator phase, the validator's gas/s and block validation time.

Stage values come from Prometheus histogram averages that base-bench scrapes about once a second,
so each block carries the average of its scrape interval. The script averages them over measured
blocks; per-block exact stages are in the builder's `native payload build timings` debug log.

With `--b`, the runs passed positionally form arm A and those after `--b` form arm B. The script
prints each arm's median per row and B's change from A. Interleave the runs (A B B A ...) on the
same host, snapshot and workload so drift lands on both arms; see etc/systems/README.md.

    etc/scripts/local/bench_ab_report.py results/a-1 results/a-2 --b results/b-1 results/b-2
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from pathlib import Path

PREFIX = "reth_base_builder_native_payload_"
STAGES = ["execution", "state_root_wait", "finish", "seal"]
INSERT = "reth_consensus_engine_beacon_block_insert_total_duration_avg"
SAVE = "reth_consensus_engine_persistence_save_blocks_duration_seconds_avg"
BATCH = "reth_consensus_engine_persistence_save_blocks_batch_size_avg"
BACKPRESSURE = "reth_consensus_engine_beacon_backpressure_active"
VALIDATION = "reth_sync_block_validation_total_duration_avg"


def mean_over_blocks(blocks: list[dict], key: str) -> float | None:
    values = [b["prometheus_metrics"][key] for b in blocks if key in b["prometheus_metrics"]]
    return statistics.fmean(values) if values else None


def ms(value: float | None) -> float | None:
    return None if value is None else value * 1e3


def gas_per_second(blocks: list[dict], interval_ms: int) -> float | None:
    if not blocks:
        return None
    return sum(b["gas_used"] for b in blocks) / (len(blocks) * interval_ms / 1e3)


def summarize(run_dir: Path) -> dict[str, float | None]:
    result = json.loads((run_dir / "benchmark-result.json").read_text())
    interval_ms = result["block_interval_ms"]
    blocks = result["blocks"]
    row: dict[str, float | None] = {
        "blocks": len(blocks),
        "block interval ms": interval_ms,
        "gas/s (M)": (gas_per_second(blocks, interval_ms) or 0) / 1e6,
        "gas/block (M)": statistics.fmean(b["gas_used"] for b in blocks) / 1e6 if blocks else None,
    }
    build = ms(mean_over_blocks(blocks, f"{PREFIX}build_duration_avg"))
    stages = [ms(mean_over_blocks(blocks, f"{PREFIX}{s}_duration_avg")) for s in STAGES]
    for stage, value in zip(STAGES, stages):
        row[f"build: {stage} ms"] = value
    row["build: total ms"] = build
    if build is not None and None not in stages:
        row["build: total - stages ms"] = build - sum(stages)
    row["engine insert ms"] = ms(mean_over_blocks(blocks, INSERT))
    save, batch = mean_over_blocks(blocks, SAVE), mean_over_blocks(blocks, BATCH)
    if save is not None and batch:
        per_block = save / batch * 1e3
        row["persistence per block ms"] = per_block
        row["persistence keeps up"] = float(per_block <= interval_ms)
    pressure = [b["prometheus_metrics"].get(BACKPRESSURE) for b in blocks]
    row["backpressure max"] = max((p for p in pressure if p is not None), default=None)
    validator = result.get("validator_blocks") or []
    if validator:
        row["validator gas/s (M)"] = (gas_per_second(validator, interval_ms) or 0) / 1e6
        row["validator validation ms"] = ms(mean_over_blocks(validator, VALIDATION))
    return row


def fmt(value: float | None) -> str:
    if value is None:
        return "-"
    return str(value) if isinstance(value, int) else f"{value:.1f}"


def print_table(header: list[str], rows: dict[str, list[str]]) -> None:
    width = max(len(k) for k in rows)
    cols = [max(len(h), *(len(r[i]) for r in rows.values())) for i, h in enumerate(header)]
    print(" " * width + "  " + "  ".join(h.rjust(c) for h, c in zip(header, cols)))
    for key, values in rows.items():
        print(key.ljust(width) + "  " + "  ".join(v.rjust(c) for v, c in zip(values, cols)))


def median(values: list[float | None]) -> float | None:
    present = [v for v in values if v is not None]
    return statistics.median(present) if present else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("runs", nargs="+", type=Path, help="arm A (or only) run directories")
    parser.add_argument("--b", nargs="+", type=Path, default=[], help="arm B run directories")
    args = parser.parse_args()

    arms = {"A": [summarize(d) for d in args.runs]}
    labels = [f"A:{d.name}" for d in args.runs]
    if args.b:
        arms["B"] = [summarize(d) for d in args.b]
        labels += [f"B:{d.name}" for d in args.b]
    summaries = [s for runs in arms.values() for s in runs]
    keys = list(dict.fromkeys(k for s in summaries for k in s))

    rows = {k: [fmt(s.get(k)) for s in summaries] for k in keys}
    header = list(labels)
    if args.b:
        header += ["median A", "median B", "B vs A"]
        for k in keys:
            a = median([s.get(k) for s in arms["A"]])
            b = median([s.get(k) for s in arms["B"]])
            known = a is not None and b is not None and abs(a) > 1e-9
            change = f"{(b - a) / a * 100:+.1f}%" if known else "-"
            rows[k] += [fmt(a), fmt(b), change]
    print_table(header, rows)
    return 0


if __name__ == "__main__":
    sys.exit(main())
