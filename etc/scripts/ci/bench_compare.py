#!/usr/bin/env python3
"""Render a criterion base-vs-head benchmark comparison into a PR comment body.

Reads two trees of criterion output (one benchmarked on the PR base commit, one
on the PR head commit), matches benchmarks by their criterion id, and emits an
advisory Markdown table of the median-time delta. This never gates a merge; it is
visibility only.

A wall-clock delta is only flagged as a regression or improvement when it clears
the percentage threshold *and* the two medians' confidence intervals do not
overlap, so ordinary run-to-run noise on a small sample does not trip the alert.
Benchmarks that produced a result on base but not head (a likely compile/run
break) are surfaced as a warning rather than silently dropped.
"""

from __future__ import annotations

from dataclasses import dataclass
import argparse
from pathlib import Path
import json

MARKER = "<!-- bench-pr-results -->"


@dataclass
class Measurement:
    """A single benchmark's median time and its confidence interval, in ns."""

    median: float
    lower: float
    upper: float


class BenchCompare:
    """Compares two criterion baselines and renders a Markdown comment."""

    def __init__(self, threshold_pct: float, improvement_pct: float) -> None:
        self.threshold_pct = threshold_pct
        self.improvement_pct = improvement_pct

    @staticmethod
    def collect(root: Path) -> dict[str, Measurement]:
        """Map criterion benchmark id -> Measurement under root.

        A criterion result lives at `<root>/<id...>/current/estimates.json`. The id
        is every path component between the artifact root and the `current` baseline
        directory, so ids are identical across the two trees regardless of how the
        upload step stripped any shared path prefix.
        """
        results: dict[str, Measurement] = {}
        for estimates in root.rglob("current/estimates.json"):
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
            # Fall back to a zero-width interval when criterion did not emit one, so
            # the overlap test degrades to a plain median comparison.
            interval = point.get("confidence_interval") or {}
            lower = float(interval.get("lower_bound", median))
            upper = float(interval.get("upper_bound", median))
            results[bench_id] = Measurement(median=median, lower=lower, upper=upper)
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
    def intervals_overlap(base: Measurement, head: Measurement) -> bool:
        """Whether the two median confidence intervals overlap.

        Overlapping intervals mean the observed delta is within run-to-run noise, so
        it must not be flagged even if the median delta clears the threshold.
        """
        return base.lower <= head.upper and head.lower <= base.upper

    def is_significant(self, base: Measurement | None, head: Measurement | None) -> bool:
        """Whether a delta is real signal: both sides present and intervals disjoint."""
        return base is not None and head is not None and not self.intervals_overlap(base, head)

    def is_regression(self, base: Measurement | None, head: Measurement | None) -> bool:
        """Regression: head slower than base beyond the threshold, outside the noise band."""
        delta = self.delta_pct(base, head)
        return delta is not None and delta >= self.threshold_pct and self.is_significant(base, head)

    def is_improvement(self, base: Measurement | None, head: Measurement | None) -> bool:
        """Improvement: head faster than base beyond the threshold, outside the noise band."""
        delta = self.delta_pct(base, head)
        return (
            delta is not None
            and delta <= -self.improvement_pct
            and self.is_significant(base, head)
        )

    def render_row(self, bench_id: str, base: Measurement | None, head: Measurement | None) -> str:
        """Render a single Markdown table row for one benchmark."""
        if base is None:
            # bench_ids is built from set(base) | set(head), so a missing base side
            # guarantees head is present; assert it so a future refactor can't turn
            # this into a silent AttributeError.
            assert head is not None
            return f"| `{bench_id}` | — (new) | {self.humanize_ns(head.median)} | — |"
        if head is None:
            return f"| `{bench_id}` | {self.humanize_ns(base.median)} | — (missing) | — |"
        delta = self.delta_pct(base, head) or 0.0
        flag = ""
        if abs(delta) >= self.threshold_pct:
            if self.is_significant(base, head):
                flag = " ⚠️ slower" if delta > 0 else " ✅ faster"
            else:
                # Threshold cleared, but the confidence intervals overlap, so treat
                # the move as noise and say so rather than raising a false alarm.
                flag = " · within noise"
        return (
            f"| `{bench_id}` | {self.humanize_ns(base.median)} | {self.humanize_ns(head.median)} "
            f"| {delta:+.1f}%{flag} |"
        )

    def summarize(
        self, base: dict[str, Measurement], head: dict[str, Measurement], ids: list[str]
    ) -> str:
        """Render a `bench (+d%)` list for an alert summary."""
        return ", ".join(f"`{b}` ({self.delta_pct(base[b], head[b]):+.1f}%)" for b in ids)

    def render(self, base_dir: Path, head_dir: Path, run_url: str) -> tuple[str, bool]:
        """Render the comment body, returning it and whether it warrants a comment."""
        base = self.collect(base_dir)
        head = self.collect(head_dir)
        bench_ids = sorted(set(base) | set(head))
        regressed = [b for b in bench_ids if self.is_regression(base.get(b), head.get(b))]
        improved = [b for b in bench_ids if self.is_improvement(base.get(b), head.get(b))]
        # Present on base but not head: the bench likely failed to compile or run on
        # the PR, which is itself a regression in coverage worth surfacing.
        dropped = [b for b in bench_ids if b in base and b not in head]

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
        lines += [
            "### Benchmark results (advisory)",
            "",
            f"Median time on the PR head versus the base branch, measured on the same "
            f"host. Wall-clock, so a change is only flagged when it clears ±"
            f"{self.threshold_pct:.0f}% *and* the confidence intervals do not overlap. "
            f"This check never blocks a merge.",
            "",
            "| Benchmark | Base | Head | Δ median |",
            "|---|---:|---:|---:|",
        ]
        if not bench_ids:
            lines.append("| _no benchmark results were produced_ | — | — | — |")
        else:
            lines.extend(
                self.render_row(b, base.get(b), head.get(b)) for b in bench_ids
            )
        lines += ["", f"[View run]({run_url}) · [Re-run benchmarks]({run_url})"]
        return "\n".join(lines) + "\n", bool(regressed or improved or dropped)


def parse_args() -> argparse.Namespace:
    """Parse CLI arguments."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-dir", type=Path, required=True)
    parser.add_argument("--head-dir", type=Path, required=True)
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
        args.base_dir, args.head_dir, args.run_url
    )
    args.output.write_text(body, encoding="utf-8")
    if args.github_output is not None:
        with args.github_output.open("a", encoding="utf-8") as handle:
            handle.write(f"should_comment={'true' if should_comment else 'false'}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
