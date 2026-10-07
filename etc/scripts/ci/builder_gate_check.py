#!/usr/bin/env python3
"""Fail the builder performance gate when a block build exceeds its pinned budget.

Reads raw iai-callgrind output from `cargo bench -p base-builder-core --bench
flashblock_build_iai` and the budgets in `etc/benchmarks/builder-gate-budgets.json`.
Each scenario fails closed: a missing measurement, a missing budget, an absolute
instruction count above `max_instructions`, or a marginal cost per deferred candidate
above `max_marginal_instructions_per_deferral` all fail the gate.

Instruction counts are deterministic for a given toolchain, dependency set, and target,
so a count above budget reflects a code change. `--pin` prints a budget file re-pinned
from the measured counts instead of checking, for intentional budget changes and
toolchain bumps (see docs/guides/BUILDER_PERFORMANCE_GATE.md).
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from iai_compare import IaiCompare  # noqa: E402

# Benchmark function -> budget-key prefix, one per builder.
BENCH_PREFIXES = {
    "flashblock_build_iai::flashblock_build::build_block/": "flashblocks/",
    "flashblock_build_iai::flashblock_build::build_native_block/": "native/",
}


class BuilderGate:
    """Checks measured instruction counts against pinned scenario budgets."""

    def __init__(self, budgets: dict, measured: dict[str, int]) -> None:
        self.budgets = budgets
        self.measured = measured
        self.failures: list[str] = []
        self.rows: list[str] = []

    @staticmethod
    def load_measured(text: str) -> dict[str, int]:
        """Map `<builder>/<scenario>` -> instruction count from iai-callgrind output."""
        measured = {}
        for bench, count in IaiCompare.parse(text).items():
            for prefix, key_prefix in BENCH_PREFIXES.items():
                if bench.startswith(prefix):
                    measured[key_prefix + bench.removeprefix(prefix)] = count
        return measured

    def marginal(self, scenario: str, spec: dict) -> float | None:
        """Instructions per deferred candidate above the reference scenario, if budgeted."""
        reference = spec.get("marginal_reference")
        if reference is None:
            return None
        if reference not in self.measured or scenario not in self.measured:
            return None
        return (self.measured[scenario] - self.measured[reference]) / spec["deferrals_per_block"]

    def check(self) -> bool:
        """Evaluate every budgeted scenario; returns True when all are within budget."""
        scenarios = self.budgets["scenarios"]
        for scenario in sorted(set(self.measured) - set(scenarios)):
            self.failures.append(f"`{scenario}` was measured but has no budget")
        for scenario, spec in scenarios.items():
            count = self.measured.get(scenario)
            budget = spec.get("max_instructions")
            baseline = spec.get("baseline_instructions")
            if count is None:
                self.failures.append(f"`{scenario}` produced no measurement")
                self.rows.append(f"| `{scenario}` | missing | {budget} | | | FAIL |")
                continue
            if budget is None:
                self.failures.append(f"`{scenario}` has no pinned instruction budget")
                self.rows.append(f"| `{scenario}` | {count:,} | unpinned | | | FAIL |")
                continue
            ok = count <= budget
            if not ok:
                self.failures.append(
                    f"`{scenario}`: {count:,} instructions exceeds budget {budget:,} "
                    f"({(count / budget - 1) * 100:+.2f}%)"
                )
            vs_baseline = f"{(count / baseline - 1) * 100:+.2f}%" if baseline else ""
            marginal = self.marginal(scenario, spec)
            marginal_cell = ""
            if marginal is not None:
                limit = spec["max_marginal_instructions_per_deferral"]
                marginal_cell = f"{marginal:,.0f} / {limit:,}"
                if marginal > limit:
                    ok = False
                    self.failures.append(
                        f"`{scenario}`: {marginal:,.0f} instructions per deferred candidate "
                        f"exceeds budget {limit:,}"
                    )
            status = "ok" if ok else "FAIL"
            self.rows.append(
                f"| `{scenario}` | {count:,} | {budget:,} | {vs_baseline} | {marginal_cell} "
                f"| {status} |"
            )
        return not self.failures

    def render(self) -> str:
        """Render a Markdown report of the check."""
        verdict = (
            "within budget"
            if not self.failures
            else f"**{len(self.failures)} budget violation(s)**"
        )
        lines = [
            f"### Builder performance gate: {verdict}",
            "",
            "| Scenario | Instructions | Budget | vs baseline | Per deferral / budget | Status |",
            "| --- | ---: | ---: | ---: | ---: | --- |",
            *self.rows,
        ]
        if self.failures:
            lines += ["", *[f"- {failure}" for failure in self.failures]]
            lines += [
                "",
                "If the increase is intended, re-pin with `etc/scripts/ci/builder_gate_check.py "
                "--pin` and justify the new budget in the PR. See "
                "`docs/guides/BUILDER_PERFORMANCE_GATE.md`.",
            ]
        return "\n".join(lines) + "\n"

    def pinned(self) -> dict:
        """Return the budget file re-pinned to the measured counts."""
        headroom = self.budgets["headroom_pct"] / 100
        marginal_headroom = self.budgets["marginal_headroom_pct"] / 100
        pinned = json.loads(json.dumps(self.budgets))
        for scenario, spec in pinned["scenarios"].items():
            count = self.measured.get(scenario)
            if count is None:
                raise SystemExit(f"cannot pin {scenario}: no measurement")
            spec["baseline_instructions"] = count
            spec["max_instructions"] = math.ceil(count * (1 + headroom))
            marginal = self.marginal(scenario, spec)
            if marginal is not None:
                spec["baseline_marginal_instructions_per_deferral"] = round(marginal)
                spec["max_marginal_instructions_per_deferral"] = math.ceil(
                    marginal * (1 + marginal_headroom)
                )
        return pinned


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", required=True, type=Path, help="raw iai-callgrind output")
    parser.add_argument("--budgets", required=True, type=Path)
    parser.add_argument("--summary", type=Path, help="append the Markdown report here")
    parser.add_argument("--pin", type=Path, help="write a re-pinned budget file here")
    args = parser.parse_args()

    budgets = json.loads(args.budgets.read_text())
    gate = BuilderGate(budgets, BuilderGate.load_measured(args.results.read_text()))
    if args.pin:
        args.pin.write_text(json.dumps(gate.pinned(), indent=2) + "\n")
        print(f"wrote re-pinned budgets to {args.pin}")
        return 0

    ok = gate.check()
    report = gate.render()
    print(report)
    if args.summary:
        with args.summary.open("a") as summary:
            summary.write(report)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
