#!/usr/bin/env python3
"""Render base-vs-head iai-callgrind instruction counts into a PR comment body.

Instruction counts from Valgrind Cachegrind are deterministic: the same code
produces the same count on every run, so a base-vs-head delta reflects a code
change and nothing else. There is therefore no measurement noise to model — a
delta beyond the threshold is real.

The comment is kept small on the happy path. When every benchmark is within the
threshold the body is a single green line with the full table tucked inside a
collapsed `<details>`. When a benchmark moves beyond the threshold, the changed
benchmarks are surfaced in a visible table while the unchanged ones stay
collapsed, so the comment only grows when there is something to look at.

Each benchmark named with `--gate` (its displayed id) must produce a head result,
and fails the run (exit status 1) when its head count rises by `--fail-pct` or
more. A gated benchmark missing on base is skipped only when the base output says
the commit lacks it (the GATED_ABSENT line run_iai_subset.sh prints, e.g. in the
PR that introduces it); otherwise its base run failed, and the gate fails too.
"""

from __future__ import annotations

import argparse
import re
from dataclasses import dataclass
from pathlib import Path

MARKER = "<!-- iai-bench-results -->"
# Printed by run_iai_subset.sh on a commit that does not have the gated benchmark.
GATED_ABSENT = "iai-subset: gated bench absent on this commit"

# iai-callgrind colorizes output; strip ANSI so parsing is colour-independent.
_ANSI = re.compile(r"\x1b\[[0-9;]*m")
# A benchmark header line: `mod::group::func` optionally followed by ` case:args`.
_HEADER = re.compile(r"^([A-Za-z_][\w]*(?:::[A-Za-z_][\w]*)+)(?:\s+(\w+):.*)?\s*$")
# A metric line: `  Instructions:  43148|N/A ...` — capture the current (left) value.
_INSTRUCTIONS = re.compile(r"^\s*Instructions:\s*(\d+)")


@dataclass
class Delta:
    """One benchmark's base and head instruction counts."""

    bench: str
    base: int | None
    head: int | None

    @property
    def delta_pct(self) -> float | None:
        """Percent change from base to head, or None if either side is missing."""
        if self.base is None or self.head is None or self.base == 0:
            return None
        return (self.head - self.base) / self.base * 100


class IaiCompare:
    """Parses iai-callgrind output and renders a base-vs-head PR comment."""

    def __init__(
        self, threshold_pct: float, gates: list[str] | None = None, fail_pct: float = 0.0
    ) -> None:
        self.threshold_pct = threshold_pct
        self.gates = gates or []
        self.fail_pct = fail_pct

    @staticmethod
    def parse(text: str) -> dict[str, int]:
        """Map benchmark id -> instruction count from captured iai-callgrind output.

        The id is `mod::group::func` plus the per-case suffix (`/case`) when the
        benchmark uses `#[bench::case]`/`#[benches::case]`, so ids line up across
        the base and head runs regardless of nesting.
        """
        results: dict[str, int] = {}
        current: str | None = None
        for raw in text.splitlines():
            line = _ANSI.sub("", raw)
            header = _HEADER.match(line)
            if header and "Instructions:" not in line:
                path, case = header.group(1), header.group(2)
                current = f"{path}/{case}" if case else path
                continue
            metric = _INSTRUCTIONS.match(line)
            if metric and current is not None:
                results[current] = int(metric.group(1))
                current = None
        return results

    @staticmethod
    def humanize(bench: str) -> str:
        """Drop the redundant `<file>_iai::` prefix and show a `/`-joined id."""
        head, _, rest = bench.partition("::")
        if head.endswith("_iai") and rest:
            bench = rest
        return bench.replace("::", "/")

    def rows(self, base: dict[str, int], head: dict[str, int]) -> list[Delta]:
        """Build one Delta per benchmark id present on either side, sorted by id."""
        return [
            Delta(bench, base.get(bench), head.get(bench))
            for bench in sorted(set(base) | set(head))
        ]

    def is_notable(self, row: Delta) -> bool:
        """A row worth surfacing: a threshold-crossing change or lost coverage."""
        if row.head is None:  # present on base, gone on head — a coverage regression
            return True
        delta = row.delta_pct
        return delta is not None and abs(delta) >= self.threshold_pct

    def gate_failures(
        self, base: dict[str, int], head: dict[str, int], base_absent: bool
    ) -> list[str]:
        """Reasons the gated benchmarks fail, one per failing benchmark."""
        base = {self.humanize(bench): count for bench, count in base.items()}
        head = {self.humanize(bench): count for bench, count in head.items()}
        failures = []
        for name in self.gates:
            row = Delta(name, base.get(name), head.get(name))
            if row.head is None:
                failures.append(f"`{name}` produced no result on head")
            elif row.base is None:
                if not base_absent:
                    failures.append(f"`{name}` produced no result on base, which has the bench")
            elif (row.delta_pct or 0.0) >= self.fail_pct:
                failures.append(
                    f"`{name}` instructions rose {row.delta_pct:+.2f}% "
                    f"(gate: +{self.fail_pct:g}%)"
                )
        return failures

    @staticmethod
    def _cell(value: int | None, *, missing: str) -> str:
        """Format an instruction count, or a placeholder when the side is missing."""
        return f"{value:,}" if value is not None else missing

    def render_row(self, row: Delta) -> str:
        """Render one Markdown table row for a benchmark."""
        base = self._cell(row.base, missing="—")
        head = self._cell(row.head, missing="— (missing)")
        if row.base is None:
            change = "🆕 new"
        elif row.head is None:
            change = "—"
        else:
            delta = row.delta_pct or 0.0
            flag = ""
            if abs(delta) >= self.threshold_pct:
                flag = " ⚠️" if delta > 0 else " ✅"
            change = f"{delta:+.1f}%{flag}"
        return f"| `{self.humanize(row.bench)}` | {base} | {head} | {change} |"

    def table(self, rows: list[Delta]) -> list[str]:
        """Render a Markdown table for the given rows (header + body)."""
        lines = [
            "| Benchmark | Base (target) | Head (this PR) | Δ instructions |",
            "|---|--:|--:|--:|",
        ]
        lines += [self.render_row(row) for row in rows]
        return lines

    def details(self, summary: str, rows: list[Delta]) -> list[str]:
        """Wrap a table in a collapsed `<details>` block."""
        if not rows:
            return []
        return [
            "<details>",
            f"<summary>{summary}</summary>",
            "",
            *self.table(rows),
            "",
            "</details>",
        ]

    def render(
        self, base: dict[str, int], head: dict[str, int], run_url: str, failures: list[str]
    ) -> str:
        """Render the full PR comment body, led by any gate failures."""
        rows = self.rows(base, head)
        notable = [row for row in rows if self.is_notable(row)]
        within = [row for row in rows if not self.is_notable(row) and row.base is not None]
        new = [row for row in rows if row.base is None]

        lead = f"[View run]({run_url})"
        gate = []
        if failures:
            gate = [
                "> [!CAUTION]",
                "> Benchmark gate failed:",
                *(f"> - {failure}" for failure in failures),
                "",
            ]
        if not rows:
            body = [MARKER, "", *gate, f"⚠️ No iai-callgrind results were produced. {lead}"]
            return "\n".join(body) + "\n"

        # Happy path: nothing crossed the threshold. One line, table collapsed.
        if not notable:
            compared = len(rows) - len(new)
            if new and compared == 0:
                # Introducing PR: nothing on the base branch to compare against yet.
                summary = (
                    f"📊 {len(new)} benchmark(s) added — baselines recorded (deterministic "
                    "instruction counts under Valgrind). Per-change deltas will appear on "
                    "future PRs, once these land on the base branch."
                )
            else:
                note = f" · {len(new)} new" if new else ""
                status = "Benchmark gate failed" if failures else "✅ All benchmarks green"
                summary = (
                    f"{status} — {compared} within "
                    f"±{self.threshold_pct:.0f}% (deterministic instruction counts){note}."
                )
            body = [
                MARKER,
                "",
                *gate,
                f"{summary} {lead}",
                "",
                *self.details(f"Benchmark details ({len(rows)})", rows),
                "",
            ]
            return "\n".join(body) + "\n"

        # Something changed: surface the offenders, keep the quiet ones collapsed.
        regressions = [r for r in notable if (r.delta_pct or 0) >= self.threshold_pct]
        collapsed = within + new
        body = [
            MARKER,
            "",
            *gate,
            "> [!WARNING]" if regressions else "> [!NOTE]",
            f"> {len(notable)} benchmark(s) changed by more than "
            f"±{self.threshold_pct:.0f}% — instruction counts are deterministic, so "
            "this reflects a real code change, not noise.",
            "",
            *self.table(sorted(notable, key=lambda r: abs(r.delta_pct or 0), reverse=True)),
            "",
            *self.details(
                f"{len(collapsed)} unchanged benchmark(s) (within ±{self.threshold_pct:.0f}%)",
                collapsed,
            ),
            "",
            lead,
        ]
        return "\n".join(body) + "\n"


def parse_args() -> argparse.Namespace:
    """Parse CLI arguments."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", type=Path, required=True, help="captured base iai output")
    parser.add_argument("--head", type=Path, required=True, help="captured head iai output")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--run-url", required=True)
    parser.add_argument("--threshold-pct", type=float, default=2.0)
    parser.add_argument(
        "--gate",
        action="append",
        default=[],
        help="displayed benchmark id that must run on head and must not regress; repeatable",
    )
    parser.add_argument("--fail-pct", type=float, default=3.0)
    return parser.parse_args()


def main() -> int:
    """Entry point for the iai-callgrind comparison renderer."""
    args = parse_args()
    compare = IaiCompare(args.threshold_pct, args.gate, args.fail_pct)
    base_text = args.base.read_text(encoding="utf-8")
    base = compare.parse(base_text)
    head = compare.parse(args.head.read_text(encoding="utf-8"))
    failures = compare.gate_failures(base, head, GATED_ABSENT in base_text.splitlines())
    args.output.write_text(compare.render(base, head, args.run_url, failures), encoding="utf-8")
    for failure in failures:
        print(f"::error title=Benchmark gate::{failure}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
