#!/usr/bin/env python3
"""Render repeated criterion base-vs-head benchmarks into a PR comment body.

Reads repeated base and head baselines from one criterion tree, matches benchmarks
by criterion id, and emits an advisory Markdown table of their wall-clock delta.
This never gates a merge; it is visibility only.

Each revision's point estimate is the geometric mean of its repeated medians. A
delta is only flagged when both paired comparisons clear the percentage threshold
in the same direction, the envelopes of all Criterion confidence intervals do not
overlap, and each revision's same-code repeat spread is below the threshold. Missing
repetitions and benchmarks that disappear on head are surfaced rather than silently
dropped.
"""

from __future__ import annotations

import argparse
import json
import math
from dataclasses import dataclass
from pathlib import Path
from statistics import fmean

MARKER = "<!-- bench-pr-results -->"


@dataclass
class Measurement:
    """A benchmark's aggregate median, confidence envelope, and repeat spread."""

    median: float
    lower: float
    upper: float
    repeat_spread_pct: float = 0.0
    repetitions: int = 1
    repeat_medians: tuple[float, ...] = ()


class BenchCompare:
    """Compares two criterion baselines and renders a Markdown comment."""

    def __init__(self, threshold_pct: float, improvement_pct: float) -> None:
        self.threshold_pct = threshold_pct
        self.improvement_pct = improvement_pct
        self.expected_repetitions = 1

    @staticmethod
    def collect(root: Path, baselines: str | list[str]) -> dict[str, Measurement]:
        """Aggregate repeated baselines into one Measurement per benchmark id.

        A criterion result lives at `<root>/<id...>/<baseline>/estimates.json`. The
        id is every path component between the artifact root and the baseline
        directory, so ids line up regardless of how deeply a bench nests.
        """
        if isinstance(baselines, str):
            baselines = [baselines]

        repeated: dict[str, list[Measurement]] = {}
        for baseline in baselines:
            for estimates in root.rglob("estimates.json"):
                if estimates.parent.name != baseline:
                    continue
                bench_id = "/".join(estimates.relative_to(root).parts[:-2])
                if not bench_id:
                    continue
                try:
                    payload = json.loads(estimates.read_text(encoding="utf-8"))
                except (json.JSONDecodeError, OSError):
                    continue
                point = payload.get("median") or payload.get("mean")
                if not point or "point_estimate" not in point:
                    continue
                median = float(point["point_estimate"])
                if median <= 0:
                    continue
                # Fall back to a zero-width interval when Criterion did not emit
                # one, so the overlap test degrades to a median comparison.
                interval = point.get("confidence_interval") or {}
                repeated.setdefault(bench_id, []).append(
                    Measurement(
                        median=median,
                        lower=float(interval.get("lower_bound", median)),
                        upper=float(interval.get("upper_bound", median)),
                    )
                )

        results: dict[str, Measurement] = {}
        for bench_id, measurements in repeated.items():
            medians = [measurement.median for measurement in measurements]
            results[bench_id] = Measurement(
                # Ratios are multiplicative, so a geometric mean treats equal
                # percentage changes in either direction symmetrically.
                median=math.exp(fmean(math.log(median) for median in medians)),
                # Enveloping each run's interval incorporates both Criterion's
                # within-run uncertainty and between-run environmental drift.
                lower=min(measurement.lower for measurement in measurements),
                upper=max(measurement.upper for measurement in measurements),
                repeat_spread_pct=(max(medians) / min(medians) - 1.0) * 100,
                repetitions=len(measurements),
                repeat_medians=tuple(medians),
            )
        return results

    @staticmethod
    def humanize_ns(value: float) -> str:
        """Format a nanosecond duration with an appropriate unit."""
        for unit, scale in (("s", 1e9), ("ms", 1e6), ("µs", 1e3)):
            if value >= scale:
                return f"{value / scale:.2f} {unit}"
        return f"{value:.1f} ns"

    def delta_pct(self, base: Measurement | None, head: Measurement | None) -> float | None:
        """Percent change from base to head median, or None if either side is missing."""
        if base is None or head is None or not base.median:
            return None
        return (head.median - base.median) / base.median * 100

    @staticmethod
    def paired_deltas(base: Measurement, head: Measurement) -> list[float]:
        """Return nearby head/base percentage deltas in baseline argument order."""
        return [
            (head_median / base_median - 1.0) * 100
            for base_median, head_median in zip(
                base.repeat_medians, head.repeat_medians, strict=False
            )
        ]

    @staticmethod
    def intervals_overlap(base: Measurement, head: Measurement) -> bool:
        """Whether the two repeated-run confidence envelopes overlap.

        Overlapping envelopes mean the observed delta is within measured noise, so
        it must not be flagged even when the aggregate median clears the threshold.
        """
        return base.lower <= head.upper and head.lower <= base.upper

    def has_all_repetitions(
        self, base: Measurement | None, head: Measurement | None
    ) -> bool:
        """Whether both sides produced every expected ABBA repetition."""
        return (
            base is not None
            and head is not None
            and base.repetitions == self.expected_repetitions
            and head.repetitions == self.expected_repetitions
        )

    def has_stable_repetitions(
        self, base: Measurement | None, head: Measurement | None
    ) -> bool:
        """Whether same-code repeat spread is below the performance threshold."""
        if base is None or head is None:
            return False
        max_spread = min(self.threshold_pct, self.improvement_pct)
        return base.repeat_spread_pct < max_spread and head.repeat_spread_pct < max_spread

    def is_significant(self, base: Measurement | None, head: Measurement | None) -> bool:
        """Whether a delta survived the coverage and measured-noise checks."""
        return (
            base is not None
            and head is not None
            and self.has_all_repetitions(base, head)
            and self.has_stable_repetitions(base, head)
            and not self.intervals_overlap(base, head)
        )

    def is_regression(self, base: Measurement | None, head: Measurement | None) -> bool:
        """Regression: both paired head runs are slower beyond the threshold."""
        if base is None or head is None or not self.is_significant(base, head):
            return False
        deltas = self.paired_deltas(base, head)
        return len(deltas) == self.expected_repetitions and all(
            delta >= self.threshold_pct for delta in deltas
        )

    def is_improvement(self, base: Measurement | None, head: Measurement | None) -> bool:
        """Improvement: both paired head runs are faster beyond the threshold."""
        if base is None or head is None or not self.is_significant(base, head):
            return False
        deltas = self.paired_deltas(base, head)
        return len(deltas) == self.expected_repetitions and all(
            delta <= -self.improvement_pct for delta in deltas
        )

    def is_notable(self, base: Measurement | None, head: Measurement | None) -> bool:
        """Whether a benchmark earns a table row.

        A row is shown for a coverage change, unstable same-code repetitions, or
        when the aggregate median clears the threshold in either direction. A
        within-noise move remains visible with its caveat, while stable results
        comfortably inside the threshold are omitted to keep the comment small.
        """
        if base is None or head is None:
            return True
        if not self.has_all_repetitions(base, head) or not self.has_stable_repetitions(base, head):
            return True
        delta = self.delta_pct(base, head)
        return delta is not None and (
            delta >= self.threshold_pct or delta <= -self.improvement_pct
        )

    def format_measurement(self, measurement: Measurement) -> str:
        """Format a measurement and mark incomplete repetitions."""
        suffix = ""
        if measurement.repetitions != self.expected_repetitions:
            suffix = f" ({measurement.repetitions}/{self.expected_repetitions})"
        return f"{self.humanize_ns(measurement.median)}{suffix}"

    @staticmethod
    def format_repeat_spread(base: Measurement | None, head: Measurement | None) -> str:
        """Format base/head same-code spread for the A/A noise column."""
        if base is None or head is None:
            return "—"
        return f"{base.repeat_spread_pct:.1f}% / {head.repeat_spread_pct:.1f}%"

    def render_row(self, bench_id: str, base: Measurement | None, head: Measurement | None) -> str:
        """Render a single Markdown table row for one benchmark."""
        if base is None:
            # bench_ids is built from set(base) | set(head), so a missing base side
            # guarantees head is present; assert it so a future refactor can't turn
            # this into a silent AttributeError.
            assert head is not None
            return (
                f"| `{bench_id}` | — (new) | {self.format_measurement(head)} | — | — |"
            )
        if head is None:
            return (
                f"| `{bench_id}` | {self.format_measurement(base)} | — (missing) | — | — |"
            )
        delta = self.delta_pct(base, head) or 0.0
        flag = ""
        if not self.has_all_repetitions(base, head):
            flag = " · incomplete"
        elif not self.has_stable_repetitions(base, head):
            flag = " · unstable repeats"
        elif self.is_regression(base, head):
            flag = " ⚠️ slower"
        elif self.is_improvement(base, head):
            flag = " ✅ faster"
        elif delta >= self.threshold_pct or delta <= -self.improvement_pct:
            flag = (
                " · within noise"
                if self.intervals_overlap(base, head)
                else " · inconsistent pairs"
            )
        return (
            f"| `{bench_id}` | {self.format_measurement(base)} | "
            f"{self.format_measurement(head)} | {delta:+.1f}%{flag} | "
            f"{self.format_repeat_spread(base, head)} |"
        )

    def summarize(
        self, base: dict[str, Measurement], head: dict[str, Measurement], ids: list[str]
    ) -> str:
        """Render a `bench (+d%)` list for an alert summary."""
        return ", ".join(f"`{b}` ({self.delta_pct(base[b], head[b]):+.1f}%)" for b in ids)

    def render(
        self,
        criterion_dir: Path,
        base_baselines: str | list[str],
        head_baselines: str | list[str],
        run_url: str,
    ) -> tuple[str, bool]:
        """Render the comment body, returning it and whether it warrants a comment."""
        if isinstance(base_baselines, str):
            base_baselines = [base_baselines]
        if isinstance(head_baselines, str):
            head_baselines = [head_baselines]
        self.expected_repetitions = max(len(base_baselines), len(head_baselines))

        base = self.collect(criterion_dir, base_baselines)
        head = self.collect(criterion_dir, head_baselines)
        bench_ids = sorted(set(base) | set(head))
        regressed = [b for b in bench_ids if self.is_regression(base.get(b), head.get(b))]
        improved = [b for b in bench_ids if self.is_improvement(base.get(b), head.get(b))]
        # Present on base but not head: the bench likely failed to compile or run on
        # the PR, which is itself a regression in coverage worth surfacing.
        dropped = [b for b in bench_ids if b in base and b not in head]
        incomplete = [
            b
            for b in bench_ids
            if b in base and b in head and not self.has_all_repetitions(base[b], head[b])
        ]

        lines = [MARKER, ""]
        # A dropped bench (lost coverage) and a regression are both actionable, so
        # show each block that applies; an improvement is only highlighted when
        # nothing regressed or dropped.
        if dropped:
            lines += [
                "> [!WARNING]",
                f"> {len(dropped)} benchmark(s) produced a result on the base branch "
                f"but not on this PR — they may have failed to compile or run: "
                f"{', '.join(f'`{b}`' for b in dropped)}.",
                "",
            ]
        if incomplete:
            lines += [
                "> [!WARNING]",
                f"> {len(incomplete)} benchmark(s) did not complete every ABBA repetition: "
                f"{', '.join(f'`{b}`' for b in incomplete)}.",
                "",
            ]
        if regressed:
            lines += [
                "> [!CAUTION]",
                f"> This PR may regress performance. {len(regressed)} benchmark(s) "
                f"slower by more than {self.threshold_pct:.0f}% beyond the noise band: "
                f"{self.summarize(base, head, regressed)}.",
                "",
            ]
        elif improved and not dropped:
            lines += [
                "> [!TIP]",
                f"> Nice, this PR improves performance. {len(improved)} benchmark(s) "
                f"faster by more than {self.improvement_pct:.0f}% beyond the noise band: "
                f"{self.summarize(base, head, improved)}.",
                "",
            ]
        # Only the notable benches get a row; unchanged ones are the bulk of the
        # subset and only add noise. The rest are summarized as an omitted count so
        # the trim is never silent.
        notable = [b for b in bench_ids if self.is_notable(base.get(b), head.get(b))]
        omitted = len(bench_ids) - len(notable)

        lines += [
            "### Benchmark results (advisory)",
            "",
            "Geometric-mean time from two base-head-head-base (ABBA) runs on one pinned "
            "CPU. A wall-clock change is only flagged when both paired comparisons "
            "clear the threshold in the same direction, the repeated-run confidence "
            "envelopes do not overlap, and each side's same-code A/A spread is below "
            f"{self.threshold_pct:.0f}%. Only notable or incomplete results are listed. "
            "This check never blocks a merge.",
            "",
            "| Benchmark | Base | Head | Δ geometric mean | A/A spread (base / head) |",
            "|---|---:|---:|---:|---:|",
        ]
        if not bench_ids:
            lines.append("| _no benchmark results were produced_ | — | — | — | — |")
        elif not notable:
            lines.append(
                f"| _all {len(bench_ids)} benchmark(s) within ±{self.threshold_pct:.0f}%_ "
                f"| — | — | — | — |"
            )
        else:
            lines.extend(
                self.render_row(b, base.get(b), head.get(b)) for b in notable
            )
        if omitted:
            lines += [
                "",
                f"_{omitted} benchmark(s) within ±{self.threshold_pct:.0f}% omitted._",
            ]
        lines += ["", f"[View run]({run_url}) · [Re-run benchmarks]({run_url})"]
        return "\n".join(lines) + "\n", bool(
            regressed or improved or dropped or incomplete
        )


def parse_args() -> argparse.Namespace:
    """Parse CLI arguments."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--criterion-dir", type=Path, required=True)
    parser.add_argument("--base-baseline", action="append", dest="base_baselines")
    parser.add_argument("--head-baseline", action="append", dest="head_baselines")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--run-url", required=True)
    parser.add_argument("--threshold-pct", type=float, default=10.0)
    parser.add_argument("--improvement-pct", type=float, default=10.0)
    parser.add_argument("--github-output", type=Path)
    return parser.parse_args()


def main() -> int:
    """Entry point for the benchmark comparison renderer."""
    args = parse_args()
    body, should_comment = BenchCompare(args.threshold_pct, args.improvement_pct).render(
        args.criterion_dir,
        args.base_baselines or ["pr-base-1", "pr-base-2"],
        args.head_baselines or ["pr-head-1", "pr-head-2"],
        args.run_url,
    )
    args.output.write_text(body, encoding="utf-8")
    if args.github_output is not None:
        with args.github_output.open("a", encoding="utf-8") as handle:
            handle.write(f"should_comment={'true' if should_comment else 'false'}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
