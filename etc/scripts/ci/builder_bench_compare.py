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

    @staticmethod
    def short(sha: str) -> str:
        return f"`{sha[:12]}`"

    def compared_commits(self, base_sha: str, head_sha: str, pr_head_sha: str) -> str:
        """The commits this run measured, or an empty string when none were given."""
        if not base_sha or not head_sha:
            return ""
        head = f"head {self.short(head_sha)}"
        if pr_head_sha and pr_head_sha != head_sha:
            head += f" (test merge of PR head {self.short(pr_head_sha)})"
        return f"Compared base {self.short(base_sha)} against {head}."

    def render(
        self,
        base: dict[str, Measurement],
        head: dict[str, Measurement],
        run_url: str,
        base_sha: str = "",
        head_sha: str = "",
        pr_head_sha: str = "",
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
        # Event counts are comparable only if the base run produced event lines at all.
        base_has_events = any(row.base.has_events for row in rows)
        header = [
            "| Builder | Scenario | Base | Head | Change | Per deferral (base → head) | Event counts |",
            "| --- | --- | ---: | ---: | ---: | ---: | --- |",
        ]
        table: list[str] = []
        flagged_lines: list[str] = []
        # Both head steps are continue-on-error, so a head benchmark or event-test case that fails
        # shows up as a base measurement with no head counterpart.
        missing = 0
        # Scenarios measured on head only, which have nothing to be compared against.
        new = 0
        # Scenarios measured on both sides whose instructions, per-deferral cost or events moved.
        moved = 0
        for row in rows:
            base_ref = base.get(f"{row.builder}/{REFERENCE_SCENARIO}")
            head_ref = head.get(f"{row.builder}/{REFERENCE_SCENARIO}")
            change = self.pct(row.base.instructions, row.head.instructions)
            base_marginal = self.per_deferral(row.base, base_ref)
            head_marginal = self.per_deferral(row.head, head_ref)
            marginal_change = self.pct(base_marginal, head_marginal)
            events = self.event_changes(row.base, row.head)
            missing_instructions = row.base.instructions is not None and row.head.instructions is None
            missing_events = row.base.has_events and not row.head.has_events
            new_instructions = row.base.instructions is None and row.head.instructions is not None
            # Head event lines are new where base has none to compare against: the base run produced
            # event lines but not this one, or this scenario has no base results at all.
            new_events = (
                row.head.has_events
                and not row.base.has_events
                and (base_has_events or row.base.instructions is None)
            )
            is_moved = (
                (change is not None and abs(change) > self.threshold_pct)
                or (marginal_change is not None and abs(marginal_change) > self.threshold_pct)
                or bool(events)
            )
            is_missing = missing_instructions or missing_events
            is_new = new_instructions or new_events
            moved += is_moved
            missing += is_missing
            new += is_new
            if missing_events:
                events = "<br>".join(filter(None, [events, "**head event test failed**"]))
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
            if missing_instructions:
                head_cell = f"**{head_cell}**"
                change_cell = "missing"
            elif is_new:
                change_cell = "new"
            else:
                change_cell = self.fmt_pct(change)
            flagged = is_moved or is_missing or is_new
            if flagged:
                change_cell = f"**{change_cell or 'changed'}**"
            line = (
                f"| {row.builder} | `{row.scenario}` | {base_cell} | {head_cell} | {change_cell} "
                f"| {marginal_cell} | {events} |"
            )
            table.append(line)
            if flagged:
                flagged_lines.append(line)

        no_events = "Event counts could not be compared because the base run produced no event lines."
        if not any(row.head.instructions is not None for row in rows):
            sentences = ["no head results were produced. See the run log."]
        elif not any(row.base.instructions is not None for row in rows):
            sentences = [
                "the base commit has no instruction counts (the benchmarks are new or failed to build "
                "there), "
                "so this run only reports head counts."
            ]
        elif moved or missing or new:
            sentences = []
            if missing:
                sentences.append(
                    f"{missing} scenario(s) have base results but no head results; see the run log."
                )
            if moved:
                sentences.append(
                    f"{moved} scenario(s) changed by more than {self.threshold_pct:g}% or changed "
                    "event counts."
                )
        elif base_has_events:
            sentences = [
                f"every scenario is within {self.threshold_pct:g}% of the base commit, with "
                "unchanged event counts."
            ]
        else:
            sentences = [f"every scenario is within {self.threshold_pct:g}% of the base commit."]
        # These apply whichever case above described the run.
        if new:
            sentences.append(f"{new} scenario(s) are new on the head commit.")
        if not base_has_events:
            sentences.append(no_events)
        if moved or missing or new:
            sentences.append("Advisory; does not block merge.")
        headline = " ".join(sentences)

        lines = [MARKER, "", f"**Builder benchmarks:** {headline}", ""]
        commits = self.compared_commits(base_sha, head_sha, pr_head_sha)
        if commits:
            lines += [commits, ""]
        lines += [
            "Instruction counts of one block build under Callgrind, base commit against head. "
            "Per deferral is the instructions per deferred candidate above the same builder's "
            "`transfers` scenario.",
            "",
        ]
        if flagged_lines:
            lines += [*header, *flagged_lines, ""]
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
    parser.add_argument("--base-sha", default="", help="commit the base results came from")
    parser.add_argument("--head-sha", default="", help="commit the head results came from")
    parser.add_argument(
        "--pr-head-sha", default="", help="PR head commit, when head is its test merge commit"
    )
    parser.add_argument("--threshold-pct", type=float, default=2.0)
    args = parser.parse_args()

    def read(path: Path) -> str:
        return path.read_text(errors="replace") if path.exists() else ""

    compare = BuilderBenchCompare(args.threshold_pct)
    base = compare.load(read(args.base_iai), read(args.base_events))
    head = compare.load(read(args.head_iai), read(args.head_events))
    args.output.write_text(
        compare.render(base, head, args.run_url, args.base_sha, args.head_sha, args.pr_head_sha)
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
