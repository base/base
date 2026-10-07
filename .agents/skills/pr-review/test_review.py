#!/usr/bin/env python3
"""Unit tests for review.py. Run with `python3 .agents/skills/pr-review/test_review.py`."""

import tempfile
import unittest
from pathlib import Path

import review

DIFF = """\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 keep
-old
+new
+added
 tail
diff --git a/src/gone.rs b/src/gone.rs
deleted file mode 100644
--- a/src/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-x
-y
"""


def thread(thread_id: str, *, resolved: bool = False, bot: bool = True) -> dict:
    return {"thread_id": thread_id, "resolved": resolved, "outdated": False, "path": "src/a.rs",
            "line": 2, "owned_by_bot": bot, "comments": []}


class AgentTests(unittest.TestCase):
    def test_checked_in_agents_load(self) -> None:
        agents = review.load_agents()
        self.assertEqual({a.stage for a in agents}, {"triage", "review", "decide"})
        for agent in agents:
            self.assertTrue((review.SCHEMAS_DIR / f"{agent.stage}.json").exists(), agent.name)
            self.assertTrue(agent.prompt, agent.name)

    def test_parse_agent_reads_front_matter_and_prompt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.md"
            path.write_text("---\nstage: review\nwhen: deep, block-production\nmodel: m\n"
                            "max_budget_usd: 2.5\n---\nDo the thing.\n")
            agent = review.parse_agent(path)
        self.assertEqual(agent.when, ("deep", "block-production"))
        self.assertEqual((agent.model, agent.effort, agent.max_budget_usd), ("m", "high", 2.5))
        self.assertEqual(agent.prompt, "Do the thing.")

    def test_parse_agent_rejects_bad_files(self) -> None:
        bad = {
            "no front matter": "just text",
            "missing model": "---\nstage: review\n---\nx",
            "bad stage": "---\nstage: nope\nmodel: m\n---\nx",
            "bad when": "---\nstage: review\nwhen: sometimes\nmodel: m\n---\nx",
            "when on triage": "---\nstage: triage\nwhen: deep\nmodel: m\n---\nx",
        }
        for label, text in bad.items():
            with self.subTest(label), tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "x.md"
                path.write_text(text)
                with self.assertRaises(review.ReviewError):
                    review.parse_agent(path)

    def test_load_agents_requires_one_triage_and_decide(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            (directory / "r.md").write_text("---\nstage: review\nmodel: m\n---\nx")
            with self.assertRaises(review.ReviewError):
                review.load_agents(directory)

    def test_select_reviewers(self) -> None:
        agents = review.load_agents()

        def names(triage: dict) -> set[str]:
            return {a.name for a in review.select_reviewers(agents, triage)}

        self.assertEqual(names({"depth": "standard", "block_production_sensitive": False}),
                         {"review-general"})
        self.assertEqual(names({"depth": "deep", "block_production_sensitive": False}),
                         {"review-general", "review-deep"})
        self.assertEqual(names({"depth": "standard", "block_production_sensitive": True}),
                         {"review-general", "review-block-production"})


class DiffTests(unittest.TestCase):
    def test_diff_new_lines(self) -> None:
        self.assertEqual(review.diff_new_lines(DIFF), {
            ("src/a.rs", 1), ("src/a.rs", 2), ("src/a.rs", 3), ("src/a.rs", 4)})

    def test_deleted_file_has_no_anchors(self) -> None:
        self.assertFalse({p for p, _ in review.diff_new_lines(DIFF)} & {"src/gone.rs"})


class PlanTests(unittest.TestCase):
    def setUp(self) -> None:
        self.valid = review.diff_new_lines(DIFF)
        self.threads = [thread("open"), thread("done", resolved=True), thread("human", bot=False)]

    def plan(self, *actions: dict, summary: str | None = "s") -> review.Plan:
        return review.build_plan({"actions": list(actions), "summary": summary, "dropped": []},
                                 self.threads, self.valid)

    def test_comment_on_diff_line_is_inline(self) -> None:
        plan = self.plan({"type": "comment", "path": "src/a.rs", "line": 3, "body": "bug",
                          "severity": "major"})
        self.assertEqual(len(plan.inline), 1)
        self.assertTrue(plan.inline[0]["body"].startswith(review.MARKER))
        self.assertIn("**Major:** bug", plan.inline[0]["body"])

    def test_comment_outside_diff_moves_to_summary(self) -> None:
        plan = self.plan({"type": "comment", "path": "src/a.rs", "line": 99, "body": "bug"},
                         {"type": "comment", "body": "no anchor"})
        self.assertEqual(plan.inline, [])
        self.assertEqual(len(plan.unanchored), 2)

    def test_inline_comments_are_capped(self) -> None:
        actions = [{"type": "comment", "path": "src/a.rs", "line": 1, "body": str(i)}
                   for i in range(review.MAX_INLINE_COMMENTS + 5)]
        plan = self.plan(*actions)
        self.assertEqual(len(plan.inline), review.MAX_INLINE_COMMENTS)
        self.assertEqual(len(plan.unanchored), 5)

    def test_reply_and_unresolve(self) -> None:
        plan = self.plan({"type": "reply", "thread_id": "open", "body": "more"},
                         {"type": "unresolve", "thread_id": "done", "body": "still broken"})
        self.assertEqual([r["thread_id"] for r in plan.replies], ["open"])
        self.assertEqual([r["thread_id"] for r in plan.unresolves], ["done"])
        self.assertEqual(plan.rejected, [])

    def test_invalid_thread_actions_are_rejected(self) -> None:
        plan = self.plan({"type": "reply", "thread_id": "human", "body": "x"},
                         {"type": "reply", "thread_id": "missing", "body": "x"},
                         {"type": "unresolve", "thread_id": "open", "body": "x"},
                         {"type": "unresolve", "body": "x"})
        self.assertEqual((plan.replies, plan.unresolves), ([], []))
        self.assertEqual(len(plan.rejected), 4)


class ThreadTests(unittest.TestCase):
    def test_parse_threads_marks_bot_ownership(self) -> None:
        def node(thread_id: str, login: str, body: str) -> dict:
            return {"id": thread_id, "isResolved": False, "isOutdated": False, "path": "a", "line": 1,
                    "comments": {"nodes": [{"author": {"login": login}, "body": body}]}}

        payload = {"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": [
            node("bot", "github-actions", "found a bug"),
            node("marked", "someone", f"{review.MARKER}\nbug"),
            node("human", "alice", "nit"),
            {**node("empty", "x", ""), "comments": {"nodes": []}},
        ]}}}}}
        owned = {t["thread_id"]: t["owned_by_bot"] for t in review.parse_threads(payload)}
        self.assertEqual(owned, {"bot": True, "marked": True, "human": False})


if __name__ == "__main__":
    unittest.main()
