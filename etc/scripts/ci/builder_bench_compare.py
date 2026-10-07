#!/usr/bin/env python3
"""Render base-vs-head builder benchmark results into a PR comment body.

Reads, for the base and head commits of a PR:

- raw iai-callgrind output from `cargo bench -p base-builder-core --bench flashblock_build_iai`
  (instruction counts per builder and scenario), and
- raw output from `cargo test -p base-builder-core --test flashblock_build_events -- --nocapture`
  (one `BUILDER_BENCH {json}` line per builder and scenario with event counts and deferrals).

For each `<builder>/<scenario>` the comment shows the instruction count, the change from base,
the cost per deferred candidate above the same builder's `transfers` scenario, and any change in
event counts. The comment is advisory: this script never fails on a regression. A run with no
results at all, or benchmark ids it does not recognize, is reported in the comment rather than
silently dropped. See docs/guides/BUILDER_BENCHMARKS.md.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from iai_compare import IaiCompare  # noqa: E402

MARKER = "<!-- builder-bench-results -->"

# Benchmark function -> builder name.
BENCH_PREFIXES = {
    "flashblock_build_iai::flashblock_build::build_block/": "flashblocks",
    "flashblock_build_iai::flashblock_build::build_native_block/": "native",
}

# Scenario whose cost every other scenario of the same builder is measured against.
REFERENCE_SCENARIO = "transfers"

_ANSI = re.compile(r"\x1b\[[0-9;]*m")
_EVENT_LINE = re.compile(r"BUILDER_BENCH (\{.*\})\s*$")


@dataclass
class Measurement:
    """One side (base or head) of one `<builder>/<scenario>` key."""

    instructions: int | None = None
    deferrals: int | None = None
    events: dict[str, int] = field(default_factory=dict)
    has_events: bool = False


@dataclass
class Row:
    """Base and head measurements for one `<builder>/<scenario>` key."""

    key: str
    base: Measurement
    head: Measurement

    @property
    def builder(self) -> str:
        return self.key.split("/", 1)[0]

    @property
    def scenario(self) -> str:
        return self.key.split("/", 1)[1]


class BuilderBenchCompare:
    """Parses base and head results and renders the advisory PR comment."""

    def __init__(self, threshold_pct: float) -> None:
        self.threshold_pct = threshold_pct
        self.unknown: list[str] = []

    def load(self, iai_text: str, events_text: str) -> dict[str, Measurement]:
        """Map `<builder>/<scenario>` -> measurement from one commit's raw outputs."""
        measured: dict[str, Measurement] = {}
        for bench, count in IaiCompare.parse(iai_text).items():
            for prefix, builder in BENCH_PREFIXES.items():
                if bench.startswith(prefix):
                    key = f"{builder}/{bench.removeprefix(prefix)}"
                    measured.setdefault(key, Measurement()).instructions = count
                    break
            else:
                if bench not in self.unknown:
                    self.unknown.append(bench)
        for line in _ANSI.sub("", events_text).splitlines():
            match = _EVENT_LINE.search(line)
            if not match:
                continue
            record = json.loads(match.group(1))
            entry = measured.setdefault(record["key"], Measurement())
            entry.deferrals = record["deferrals"]
            entry.events = record["events"]
            entry.has_events = True
        return measured

    @staticmethod
    def per_deferral(row_side: Measurement, reference: Measurement | None) -> float | None:
        """Instructions per deferred candidate above the builder's reference scenario."""
        if (
            reference is None
            or reference.instructions is None
            or row_side.instructions is None
            or not row_side.deferrals
        ):
            return None
        return (row_side.instructions - reference.instructions) / row_side.deferrals

    @staticmethod
    def pct(base: float | None, head: float | None) -> float | None:
        if base is None or head is None or base == 0:
            return None
        return (head - base) / base * 100

    @staticmethod
    def fmt_pct(value: float | None) -> str:
        return "" if value is None else f"{value:+.2f}%"

    @staticmethod
    def event_changes(base: Measurement, head: Measurement) -> str:
        """Event types whose count changed, as `TYPE base → head`."""
        if not base.has_events or not head.has_events:
            return ""
        changes = []
        for event_type in sorted(set(base.events) | set(head.events)):
            before = base.events.get(event_type, 0)
            after = head.events.get(event_type, 0)
            if before != after:
                name = event_type.removeprefix("BUILDER_")
                changes.append(f"{name} {before:,} → {after:,}")
        return "<br>".join(changes)

    def render(
        self, base: dict[str, Measurement], head: dict[str, Measurement], run_url: str
    ) -> str:
        """Render the comment body."""
        # Builders in `BENCH_PREFIXES` order, scenarios in the order the bench ran them.
        builders = list(BENCH_PREFIXES.values())
        seen = list(dict.fromkeys([*head, *base]))
        keys = sorted(
            seen,
            key=lambda key: (
                builders.index(key.split("/", 1)[0])
                if key.split("/", 1)[0] in builders
                else len(builders),
                seen.index(key),
            ),
        )
        rows = [Row(key, base.get(key, Measurement()), head.get(key, Measurement())) for key in keys]
        lines: list[str] = []
        header = [
            "| Builder | Scenario | Base | Head | Change | Per deferral (base → head) | Event counts |",
            "| --- | --- | ---: | ---: | ---: | ---: | --- |",
        ]
        table: list[str] = []
        changed: list[str] = []
        for row in rows:
            base_ref = base.get(f"{row.builder}/{REFERENCE_SCENARIO}")
            head_ref = head.get(f"{row.builder}/{REFERENCE_SCENARIO}")
            change = self.pct(row.base.instructions, row.head.instructions)
            base_marginal = self.per_deferral(row.base, base_ref)
            head_marginal = self.per_deferral(row.head, head_ref)
            marginal_change = self.pct(base_marginal, head_marginal)
            events = self.event_changes(row.base, row.head)
            flagged = (
                (change is not None and abs(change) > self.threshold_pct)
                or (marginal_change is not None and abs(marginal_change) > self.threshold_pct)
                or bool(events)
            )
            marginal_cell = ""
            if head_marginal is not None:
                marginal_cell = (
                    f"{base_marginal:,.0f} → {head_marginal:,.0f} ({self.fmt_pct(marginal_change)})"
                    if base_marginal is not None
                    else f"new → {head_marginal:,.0f}"
                )
            base_cell = f"{row.base.instructions:,}" if row.base.instructions is not None else "new"
            head_cell = (
                f"{row.head.instructions:,}" if row.head.instructions is not None else "missing"
            )
            change_cell = self.fmt_pct(change)
            if flagged:
                change_cell = f"**{change_cell or 'changed'}**"
            line = (
                f"| {row.builder} | `{row.scenario}` | {base_cell} | {head_cell} | {change_cell} "
                f"| {marginal_cell} | {events} |"
            )
            table.append(line)
            if flagged:
                changed.append(line)

        lines.append(MARKER)
        lines.append("")
        if not any(row.head.instructions is not None for row in rows):
            lines.append("**Builder benchmarks:** no head results were produced. See the run log.")
        elif not any(row.base.instructions is not None for row in rows):
            lines.append(
                "**Builder benchmarks:** the base commit has no results (the benchmarks are new "
                "or failed to build there), so this run only reports head counts."
            )
        elif changed:
            lines.append(
                f"**Builder benchmarks:** {len(changed)} scenario(s) changed by more than "
                f"{self.threshold_pct:g}% or changed event counts. Advisory; does not block merge."
            )
        else:
            lines.append(
                f"**Builder benchmarks:** every scenario is within {self.threshold_pct:g}% of the "
                "base commit, with unchanged event counts."
            )
        lines.append("")
        lines.append(
            "Instruction counts of one block build under Callgrind, base commit against head. "
            "Per deferral is the instructions per deferred candidate above the same builder's "
            "`transfers` scenario."
        )
        lines.append("")
        if changed:
            lines += [*header, *changed, ""]
        lines += ["<details><summary>All scenarios</summary>", "", *header, *table, "", "</details>"]
        if self.unknown:
            lines += [
                "",
                "Unrecognized benchmark ids (update `BENCH_PREFIXES` in "
                "`etc/scripts/ci/builder_bench_compare.py`): "
                + ", ".join(f"`{bench}`" for bench in self.unknown),
            ]
        lines += ["", f"[Run log]({run_url}) · `docs/guides/BUILDER_BENCHMARKS.md`"]
        return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-iai", required=True, type=Path)
    parser.add_argument("--head-iai", required=True, type=Path)
    parser.add_argument("--base-events", required=True, type=Path)
    parser.add_argument("--head-events", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--run-url", default="")
    parser.add_argument("--threshold-pct", type=float, default=2.0)
    args = parser.parse_args()

    def read(path: Path) -> str:
        return path.read_text(errors="replace") if path.exists() else ""

    compare = BuilderBenchCompare(args.threshold_pct)
    base = compare.load(read(args.base_iai), read(args.base_events))
    head = compare.load(read(args.head_iai), read(args.head_events))
    args.output.write_text(compare.render(base, head, args.run_url))
    return 0


if __name__ == "__main__":
    sys.exit(main())
