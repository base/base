#!/usr/bin/env python3
"""Compare criterion benchmarks between two git refs with an interleaved A/B/B/A schedule.

Wall-clock benchmarks drift with machine load, so a single base run followed by a
single head run confounds the code change with whatever else the machine was doing.
This script builds the benchmark binaries of a base ref (A) and a head ref (B) once
each, then runs the two binaries alternately (A B B A, repeated) and reports every
run next to the per-arm median, so a code effect has to show up consistently across
arms while load swings average out. It is the local counterpart of the advisory
`bench-pr.yml` comparison, for the builder selection-loop benchmarks whose per-PR
CI numbers are too noisy to read on their own.

Example, comparing the checked-out head against the merge base with `origin/main`
on the resting-predicate scenarios of both builders:

    etc/scripts/local/bench_abba.py --rounds 3

Each ref is checked out into a temporary git worktree with its own target dir, so
the two builds never invalidate each other. Pass `--skip-build` to reuse binaries
from an earlier run in the same `--out` directory; the directory's manifest must
name the same two commits. On Linux, `--pin` runs each arm under `taskset` on the
given CPU list.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

# Benchmarks and criterion filters of the resting-predicate scenarios. Each entry is
# `(package, bench target, filter regex)`; criterion matches the filter against the
# full benchmark id.
RESTING_BENCHES = [
    (
        "base-builder-core",
        "tx_selection",
        r"resting_blocks/.*mode=Enforce|resting_predicates/resting=10000/.*mode=Enforce"
        r"|resting_blocks/resting=20000/.*mode=Off",
    ),
    ("base-execution-payload-builder", "native_build_loop", r"mode=Enforce"),
]

# `tx_selection/resting_blocks/... time:   [1.2 s 1.3 s 1.4 s]` across two lines. The middle
# value is criterion's point estimate of the iteration time (the regression slope, or the
# mean when it has no slope), not a median.
_BENCH_ID = re.compile(r"^([a-z_]+/\S+)\s*$")
_TIME = re.compile(r"time:\s+\[(\S+) (\w+) (\S+) (\w+) (\S+) (\w+)\]")
_UNITS_TO_MS = {"ns": 1e-6, "µs": 1e-3, "us": 1e-3, "ms": 1.0, "s": 1e3}

# Records which commit each arm's binaries were built from, so `--skip-build` cannot report
# binaries of an earlier head under a new ref.
MANIFEST = "manifest.json"


@dataclass
class Arm:
    """One side of the comparison: a git ref and where its binaries live."""

    label: str
    ref: str
    out: Path
    results: dict[str, list[float]] = field(default_factory=dict)

    def binary(self, bench: str) -> Path:
        return self.out / f"{bench}-{self.label}"


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    print("+", " ".join(cmd), flush=True)
    return subprocess.run(cmd, check=True, **kwargs)


def resolve(ref: str) -> str:
    return subprocess.run(
        ["git", "rev-parse", "--verify", ref], check=True, capture_output=True, text=True
    ).stdout.strip()


def build_arm(arm: Arm, repo: Path, worktrees: Path) -> None:
    """Builds every bench target of `arm.ref` in its own worktree and target dir."""
    tree = worktrees / arm.label
    run(["git", "-C", str(repo), "worktree", "add", "--detach", str(tree), arm.ref])
    env = {
        **os.environ,
        "CARGO_TARGET_DIR": str(tree / "target"),
        # The proof crates link an SP1 ELF that is irrelevant to these benches.
        "BASE_SUCCINCT_ELF_STUB": "1",
    }
    try:
        for package, bench, _ in RESTING_BENCHES:
            out = run(
                ["cargo", "bench", "-p", package, "--bench", bench, "--no-run"],
                cwd=tree,
                env=env,
                capture_output=True,
                text=True,
            )
            executable = re.search(r"Executable .*\((\S+)\)", out.stderr + out.stdout)
            if executable is None:
                sys.exit(f"could not find the {bench} executable in cargo output")
            shutil.copy2(tree / executable.group(1), arm.binary(bench))
    finally:
        run(["git", "-C", str(repo), "worktree", "remove", "--force", str(tree)])


def run_arm(arm: Arm, pin: str | None) -> None:
    """Runs every bench target of `arm` once and records each benchmark's point estimate."""
    for _, bench, filter_regex in RESTING_BENCHES:
        cmd = [str(arm.binary(bench)), "--bench", filter_regex, "--noplot"]
        if pin:
            cmd = ["taskset", "-c", pin, *cmd]
        print("+", " ".join(cmd), flush=True)
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            print(proc.stdout, proc.stderr, file=sys.stderr)
            sys.exit(f"{bench} arm {arm.label} failed with status {proc.returncode}")
        bench_id = None
        measured = 0
        for line in proc.stdout.splitlines():
            if match := _BENCH_ID.match(line):
                bench_id = match.group(1)
            elif (match := _TIME.search(line)) and bench_id:
                estimate_ms = float(match.group(3)) * _UNITS_TO_MS[match.group(4)]
                arm.results.setdefault(bench_id, []).append(estimate_ms)
                print(f"  {bench_id}: {estimate_ms:.2f} ms", flush=True)
                bench_id = None
                measured += 1
        if measured == 0:
            print(proc.stdout, file=sys.stderr)
            sys.exit(f"{bench} arm {arm.label} matched no benchmark: check the filter and output")


def check_complete(base: Arm, head: Arm, runs_per_arm: int) -> None:
    """Fails when the arms measured different benchmarks or a benchmark lacks a run."""
    if base.results.keys() != head.results.keys():
        only_a = sorted(base.results.keys() - head.results.keys())
        only_b = sorted(head.results.keys() - base.results.keys())
        sys.exit(f"arms measured different benchmarks: only A {only_a}, only B {only_b}")
    for arm in (base, head):
        short = {k: len(v) for k, v in arm.results.items() if len(v) != runs_per_arm}
        if short:
            sys.exit(f"arm {arm.label} expected {runs_per_arm} runs per benchmark, got {short}")


def report(base: Arm, head: Arm) -> str:
    """Renders every run's point estimate, the per-arm medians and the head-vs-base delta."""
    lines = [
        f"A = {base.ref}, B = {head.ref}. Criterion point estimates per run in ms; delta",
        "compares the per-arm medians of those runs, and min compares the fastest run of each arm.",
        "",
        "| Benchmark | A median | B median | delta | min delta | A runs | B runs |",
        "|---|---:|---:|---:|---:|---|---|",
    ]
    for bench_id in base.results:
        a, b = base.results[bench_id], head.results[bench_id]
        a_median, b_median = statistics.median(a), statistics.median(b)
        delta = 100 * (b_median / a_median - 1)
        min_delta = 100 * (min(b) / min(a) - 1)
        fmt = lambda runs: " / ".join(f"{value:.1f}" for value in runs)  # noqa: E731
        lines.append(
            f"| `{bench_id}` | {a_median:.1f} | {b_median:.1f} | {delta:+.1f}% | "
            f"{min_delta:+.1f}% | {fmt(a)} | {fmt(b)} |"
        )
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--base", help="base ref (default: merge base with origin/main)")
    parser.add_argument("--head", default="HEAD", help="head ref (default: HEAD)")
    parser.add_argument("--rounds", type=int, default=2, help="A B B A rounds (default: 2)")
    parser.add_argument("--out", type=Path, help="binary and report directory")
    parser.add_argument("--skip-build", action="store_true", help="reuse binaries in --out")
    parser.add_argument("--pin", help="Linux CPU list for taskset, e.g. 4-15")
    args = parser.parse_args()

    repo = Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], check=True, capture_output=True, text=True
        ).stdout.strip()
    )
    base_ref = args.base or subprocess.run(
        ["git", "merge-base", "origin/main", args.head],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    out = args.out or Path(tempfile.mkdtemp(prefix="bench-abba-"))
    out.mkdir(parents=True, exist_ok=True)
    base = Arm("A", resolve(base_ref), out)
    head = Arm("B", resolve(args.head), out)

    manifest = out / MANIFEST
    built = {base.label: base.ref, head.label: head.ref}
    if args.skip_build:
        if not manifest.exists():
            sys.exit(f"--skip-build needs binaries built by this script in {out}")
        previous = json.loads(manifest.read_text())
        if previous != built:
            sys.exit(f"binaries in {out} were built from {previous}, not {built}; drop --skip-build")
    else:
        with tempfile.TemporaryDirectory(prefix="bench-abba-worktrees-") as worktrees:
            for arm in (base, head):
                build_arm(arm, repo, Path(worktrees))
        manifest.write_text(json.dumps(built, indent=2) + "\n")

    for _ in range(args.rounds):
        for arm in (base, head, head, base):
            print(f"=== arm {arm.label} ({arm.ref[:12]}) load={os.getloadavg()}", flush=True)
            run_arm(arm, args.pin)

    check_complete(base, head, 2 * args.rounds)
    table = report(base, head)
    (out / "report.md").write_text(table + "\n")
    print(f"\n{table}\n\nreport written to {out / 'report.md'}")


if __name__ == "__main__":
    main()
