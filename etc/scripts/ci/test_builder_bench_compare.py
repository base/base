#!/usr/bin/env python3
"""Tests for the headline and table of the builder benchmark PR comment.

Run with `python3 etc/scripts/ci/test_builder_bench_compare.py`.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from builder_bench_compare import BuilderBenchCompare, Measurement  # noqa: E402

BASE_SHA = "a" * 40
HEAD_SHA = "b" * 40
PR_HEAD_SHA = "c" * 40


def measured(instructions: int, deferrals: int = 0, events: dict[str, int] | None = None):
    """A measurement with an instruction count and, unless `events` is None, an event line."""
    return Measurement(
        instructions=instructions,
        deferrals=deferrals,
        events=events or {},
        has_events=events is not None,
    )


def baseline() -> dict[str, Measurement]:
    return {
        "flashblocks/transfers": measured(1_000_000, events={}),
        "flashblocks/resting_backlog": measured(
            3_000_000, deferrals=100, events={"BUILDER_DEFERRED": 10}
        ),
        "native/transfers": measured(500_000, events={}),
    }


def headline(body: str) -> str:
    return next(line for line in body.splitlines() if line.startswith("**Builder benchmarks:**"))


class RenderTest(unittest.TestCase):
    def render(self, base, head, pr_head_sha: str = PR_HEAD_SHA) -> str:
        return BuilderBenchCompare(threshold_pct=2.0).render(
            base, head, "https://run", BASE_SHA, HEAD_SHA, pr_head_sha
        )

    def test_all_within_threshold(self):
        body = self.render(baseline(), baseline())
        self.assertEqual(
            headline(body),
            "**Builder benchmarks:** every scenario is within 2% of the base commit, with "
            "unchanged event counts.",
        )
        self.assertNotIn("| --- |\n| flashblocks", body.split("<details>")[0])

    def test_names_compared_commits(self):
        body = self.render(baseline(), baseline())
        self.assertIn(
            "Compared base `aaaaaaaaaaaa` against head `bbbbbbbbbbbb` "
            "(test merge of PR head `cccccccccccc`).",
            body,
        )
        body = self.render(baseline(), baseline(), pr_head_sha="")
        self.assertIn("Compared base `aaaaaaaaaaaa` against head `bbbbbbbbbbbb`.", body)

    def test_new_scenario_on_head_is_counted(self):
        head = baseline()
        head["native/wake_rescan"] = measured(700_000, deferrals=10, events={"BUILDER_DEFERRED": 1})
        body = self.render(baseline(), head)
        self.assertEqual(
            headline(body),
            "**Builder benchmarks:** 1 scenario(s) are new on the head commit. "
            "Advisory; does not block merge.",
        )
        self.assertIn("| native | `wake_rescan` | new | 700,000 | **new** |", body)

    def test_base_without_events_does_not_claim_unchanged_events(self):
        base = {key: measured(m.instructions, m.deferrals) for key, m in baseline().items()}
        body = self.render(base, baseline())
        self.assertEqual(
            headline(body),
            "**Builder benchmarks:** every scenario is within 2% of the base commit. Event counts "
            "could not be compared because the base run produced no event lines.",
        )

    def test_regression_is_flagged(self):
        head = baseline()
        head["flashblocks/resting_backlog"] = measured(
            3_300_000, deferrals=100, events={"BUILDER_DEFERRED": 10}
        )
        body = self.render(baseline(), head)
        self.assertEqual(
            headline(body),
            "**Builder benchmarks:** 1 scenario(s) changed by more than 2% or changed event "
            "counts. Advisory; does not block merge.",
        )
        self.assertIn("| flashblocks | `resting_backlog` | 3,000,000 | 3,300,000 | **+10.00%** |", body)

    def test_regression_that_lost_its_event_line_counts_as_both(self):
        head = baseline()
        head["flashblocks/resting_backlog"] = measured(3_300_000, deferrals=100)
        body = self.render(baseline(), head)
        self.assertEqual(
            headline(body),
            "**Builder benchmarks:** 1 scenario(s) have base results but no head results; see the "
            "run log. 1 scenario(s) changed by more than 2% or changed event counts. "
            "Advisory; does not block merge.",
        )
        self.assertIn("**head event test failed**", body)

    def test_missing_head_instructions_is_flagged(self):
        head = baseline()
        del head["native/transfers"]
        body = self.render(baseline(), head)
        self.assertTrue(
            headline(body).startswith(
                "**Builder benchmarks:** 1 scenario(s) have base results but no head results"
            )
        )
        self.assertIn("| native | `transfers` | 500,000 | **missing** | **missing** |", body)


if __name__ == "__main__":
    unittest.main()
