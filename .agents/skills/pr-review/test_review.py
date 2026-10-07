#!/usr/bin/env python3
"""Unit tests for review.py and render.py. Run with `python3 .agents/skills/pr-review/test_review.py`."""

import json
import tempfile
import unittest
from pathlib import Path

import render
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


def thread(thread_id: str, *, resolved: bool = False, bot: bool = True, body: str = "A problem.") -> dict:
    return {"thread_id": thread_id, "url": f"https://example.test/{thread_id}", "resolved": resolved,
            "outdated": False, "path": "src/a.rs", "line": 2, "owned_by_bot": bot,
            "comments": [{"author": "github-actions", "body": body}]}


def comment(**overrides) -> dict:
    return {"type": "comment", "path": "src/a.rs", "line": 3, "title": "Panics on empty batch",
            "body": "`decode` unwraps the first item. An empty batch panics.", "severity": "major",
            "category": "error-handling", **overrides}


def write_agent(directory: Path, name: str, stage: str, extra: str = "") -> None:
    (directory / f"{name}.md").write_text(f"---\nstage: {stage}\nmodel: m\n{extra}---\nPrompt.")


class AgentTests(unittest.TestCase):
    def test_checked_in_agents_load(self) -> None:
        agents = review.load_agents()
        self.assertEqual({a.stage for a in agents}, set(review.STAGES))
        for agent in agents:
            self.assertTrue((review.SCHEMAS_DIR / f"{review.SCHEMA_FOR_STAGE[agent.stage]}.json").exists())
            self.assertTrue(agent.prompt, agent.name)
        self.assertGreaterEqual(sum(a.stage == "council" for a in agents), 2)

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

    def test_load_agents_validates_the_agent_set(self) -> None:
        base = [("t", "triage"), ("r", "review"), ("d", "decide")]
        cases = {
            "valid without council": (base, True),
            "valid with council": (base + [("m1", "council"), ("m2", "council"), ("c", "chair")], True),
            "no triage": ([("r", "review"), ("d", "decide")], False),
            "no review": ([("t", "triage"), ("d", "decide")], False),
            "one council member": (base + [("m1", "council"), ("c", "chair")], False),
            "council without chair": (base + [("m1", "council"), ("m2", "council")], False),
            "chair without council": (base + [("c", "chair")], False),
        }
        for label, (files, ok) in cases.items():
            with self.subTest(label), tempfile.TemporaryDirectory() as tmp:
                for name, stage in files:
                    write_agent(Path(tmp), name, stage)
                if ok:
                    review.load_agents(Path(tmp))
                else:
                    with self.assertRaises(review.ReviewError):
                        review.load_agents(Path(tmp))

    def test_select_reviewers(self) -> None:
        agents = review.load_agents()

        def names(triage: dict) -> set[str]:
            return {a.name for a in review.select_reviewers(agents, triage)}

        self.assertEqual(names({"depth": "standard", "block_production_sensitive": False}),
                         {"review-general"})
        self.assertEqual(names({"depth": "deep", "block_production_sensitive": False}),
                         {"review-general"})
        self.assertEqual(names({"depth": "standard", "block_production_sensitive": True}),
                         {"review-general", "review-block-production"})


class VocabularyTests(unittest.TestCase):
    """The enums in the schemas must match what render.py can display."""

    def enum_of(self, schema: str, *path: str) -> list[str]:
        node = json.loads((review.SCHEMAS_DIR / f"{schema}.json").read_text())
        for key in path:
            node = node["properties"][key]
        return node["enum"]

    def test_schema_enums_match_render(self) -> None:
        categories = [c for c in render.CATEGORIES if c != "other"]
        severities = list(render.SEVERITIES)
        for schema, prefix in (("review", ("findings", "items")), ("chair", ("findings", "items")),
                               ("decide", ("actions", "items"))):
            with self.subTest(schema):
                node = json.loads((review.SCHEMAS_DIR / f"{schema}.json").read_text())
                for key in prefix[:1]:
                    node = node["properties"][key]["items"]
                self.assertEqual(node["properties"]["severity"]["enum"], severities)
                self.assertEqual(node["properties"]["category"]["enum"], categories)

    def test_finding_guide_documents_every_category(self) -> None:
        guide = review.FINDING_GUIDE.read_text()
        for category in render.CATEGORIES:
            if category != "other":
                self.assertIn(f"`{category}`", guide)


class DiffTests(unittest.TestCase):
    def test_diff_new_lines(self) -> None:
        self.assertEqual(review.diff_new_lines(DIFF), {
            ("src/a.rs", 1), ("src/a.rs", 2), ("src/a.rs", 3), ("src/a.rs", 4)})

    def test_deleted_file_has_no_anchors(self) -> None:
        self.assertFalse({p for p, _ in review.diff_new_lines(DIFF)} & {"src/gone.rs"})


class FindingTests(unittest.TestCase):
    def test_header_and_markdown(self) -> None:
        finding = render.Finding.from_action(comment(suggestion="Return `Err` instead.",
                                                     evidence="1. call\n2. panic"))
        self.assertEqual(finding.header(), "🟠 **Major · Error handling** — Panics on empty batch")
        text = finding.markdown()
        self.assertTrue(text.startswith(finding.header() + "\n\n`decode` unwraps"))
        self.assertIn("**Suggested fix:** Return `Err` instead.", text)
        self.assertIn("<details>\n<summary>Evidence</summary>", text)

    def test_missing_or_invalid_fields_get_defaults(self) -> None:
        finding = render.Finding.from_action({"type": "comment", "body": "First line.\nSecond.",
                                              "severity": "bogus", "category": "nonsense"})
        self.assertEqual((finding.severity, finding.category), ("minor", "other"))
        self.assertEqual(finding.title, "First line.")

    def test_long_title_is_clipped(self) -> None:
        finding = render.Finding.from_action(comment(title="x" * 200))
        self.assertLessEqual(len(finding.title), render.MAX_TITLE_CHARS)

    def test_thread_header_round_trips(self) -> None:
        finding = render.Finding.from_action(comment(severity="critical", category="block-production"))
        body = f"{render.MARKER}\n{finding.markdown()}"
        severity, text = render.thread_header(thread("t", body=body))
        self.assertEqual((severity, text), ("critical", finding.header()))

    def test_thread_header_falls_back_for_old_comments(self) -> None:
        severity, text = render.thread_header(thread("t", body="Old style comment.\nMore."))
        self.assertEqual((severity, text), (None, "💬 Old style comment."))


class PlanTests(unittest.TestCase):
    def setUp(self) -> None:
        self.valid = review.diff_new_lines(DIFF)
        self.threads = [thread("open"), thread("done", resolved=True), thread("human", bot=False)]

    def plan(self, *actions: dict) -> review.Plan:
        return review.build_plan({"actions": list(actions), "overview": None, "dropped": []},
                                 self.threads, self.valid)

    def test_comment_on_diff_line_is_inline(self) -> None:
        plan = self.plan(comment())
        self.assertEqual(len(plan.new), 1)
        inline = plan.inline_comments()[0]
        self.assertEqual((inline["path"], inline["line"]), ("src/a.rs", 3))
        self.assertTrue(inline["body"].startswith(render.MARKER))

    def test_comment_outside_diff_moves_to_summary(self) -> None:
        plan = self.plan(comment(line=99), comment(path=None, line=None))
        self.assertEqual(plan.new, [])
        self.assertEqual(len(plan.outside), 2)

    def test_comments_are_sorted_by_severity_then_location(self) -> None:
        plan = self.plan(comment(severity="minor", line=1), comment(severity="critical", line=4),
                         comment(severity="major", line=2), comment(severity="major", line=1))
        self.assertEqual([(f.severity, f.line) for f in plan.new],
                         [("critical", 4), ("major", 1), ("major", 2), ("minor", 1)])

    def test_inline_comments_are_capped_keeping_the_most_severe(self) -> None:
        actions = [comment(severity="minor", line=1) for _ in range(review.MAX_INLINE_COMMENTS)]
        actions.append(comment(severity="critical", line=2))
        plan = self.plan(*actions)
        self.assertEqual(len(plan.new), review.MAX_INLINE_COMMENTS)
        self.assertEqual(plan.new[0].severity, "critical")
        self.assertEqual([f.severity for f in plan.outside], ["minor"])

    def test_reply_and_unresolve(self) -> None:
        plan = self.plan({"type": "reply", "thread_id": "open", "body": "more"},
                         {"type": "unresolve", "thread_id": "done", "body": "still broken"})
        self.assertEqual([r["thread_id"] for r in plan.replies], ["open"])
        self.assertIn("**Follow-up:** more", plan.replies[0]["body"])
        self.assertEqual([r["thread_id"] for r in plan.unresolves], ["done"])
        self.assertIn("**Reopened:** still broken", plan.unresolves[0]["body"])
        self.assertEqual(plan.rejected, [])

    def test_invalid_thread_actions_are_rejected(self) -> None:
        plan = self.plan({"type": "reply", "thread_id": "human", "body": "x"},
                         {"type": "reply", "thread_id": "missing", "body": "x"},
                         {"type": "unresolve", "thread_id": "open", "body": "x"},
                         {"type": "unresolve", "body": "x"})
        self.assertEqual((plan.replies, plan.unresolves), ([], []))
        self.assertEqual(len(plan.rejected), 4)


class SummaryTests(unittest.TestCase):
    def summary(self, plan: review.Plan, threads: list[dict] | None = None, *, failed: list[str] | None = None,
                replace_existing: bool = False) -> str | None:
        return render.render_summary(
            overview="One panic path.", new=plan.new, outside=plan.outside, threads=threads or [],
            reopened={u["thread_id"] for u in plan.unresolves}, failed=failed or [], details="Details.",
            repo="base/base", head_sha="abc123", replace_existing=replace_existing)

    def test_summary_lists_findings_with_links(self) -> None:
        plan = review.build_plan({"actions": [comment(severity="critical"), comment(line=1)]}, [],
                                 review.diff_new_lines(DIFF))
        text = self.summary(plan)
        self.assertTrue(text.startswith(render.SUMMARY_MARKER))
        self.assertIn("## 🔴 1 critical · 🟠 1 major", text)
        self.assertIn("One panic path.", text)
        self.assertIn("| 🔴 | Error handling | Panics on empty batch | "
                      "[`src/a.rs:3`](https://github.com/base/base/blob/abc123/src/a.rs#L3) |", text)
        self.assertLess(text.index("🔴 | Error"), text.index("🟠 | Error"))

    def test_pipes_in_titles_do_not_break_the_table(self) -> None:
        plan = review.build_plan({"actions": [comment(title="a | b")]}, [], review.diff_new_lines(DIFF))
        self.assertIn("a \\| b", self.summary(plan))

    def test_open_threads_count_and_are_listed(self) -> None:
        finding = render.Finding.from_action(comment())
        threads = [thread("open", body=f"{render.MARKER}\n{finding.markdown()}"),
                   thread("done", resolved=True), thread("human", bot=False)]
        text = self.summary(review.Plan(), threads)
        self.assertIn("## 🟠 1 major", text)
        self.assertIn("### Open from earlier reviews", text)
        self.assertIn("(https://example.test/open)", text)
        self.assertNotIn("example.test/human", text)
        self.assertNotIn("example.test/done", text)

    def test_reopened_thread_is_listed_separately(self) -> None:
        plan = review.build_plan({"actions": [{"type": "unresolve", "thread_id": "done", "body": "again"}]},
                                 [thread("done", resolved=True)], set())
        text = self.summary(plan, [thread("done", resolved=True)])
        self.assertIn("### Reopened", text)
        self.assertNotIn("Open from earlier reviews", text)

    def test_failed_agents_are_called_out(self) -> None:
        self.assertIn("Incomplete review:** `council/x`", self.summary(review.Plan(), failed=["council/x"]))

    def test_nothing_to_say(self) -> None:
        self.assertIsNone(self.summary(review.Plan()))
        replacing = self.summary(review.Plan(), replace_existing=True)
        self.assertIn("## ✅ No open findings", replacing)

    def test_outside_diff_findings_are_listed(self) -> None:
        plan = review.build_plan({"actions": [comment(line=99)]}, [], review.diff_new_lines(DIFF))
        text = self.summary(plan)
        self.assertIn("### Outside the diff", text)
        self.assertIn("`src/a.rs`", text)


class CouncilTests(unittest.TestCase):
    def test_merge_candidates_numbers_findings_across_members(self) -> None:
        finding = {"title": "t", "severity": "major", "category": "safety", "confidence": "high",
                   "path": "a", "line": 1, "explanation": "e"}
        drafts = {"m1": {"findings": [finding, finding]}, "m2": {"findings": [finding]}, "m3": {"findings": []}}
        candidates = review.merge_candidates(drafts)
        self.assertEqual([(c["id"], c["reported_by"]) for c in candidates],
                         [("F1", "m1"), ("F2", "m1"), ("F3", "m2")])

    def test_ballot_excludes_the_members_own_findings(self) -> None:
        ctx = review.Context(description="d", title="t", files=["a"], diff=DIFF)
        candidates = [{"id": "F1", "reported_by": "m1", "title": "mine"},
                      {"id": "F2", "reported_by": "m2", "title": "theirs"}]
        prompt = review.ballot_prompt(ctx, {}, "m1", candidates)
        self.assertIn("theirs", prompt)
        self.assertNotIn('"mine"', prompt)

    def test_run_parallel_keeps_job_order_and_collects_failures(self) -> None:
        def fail() -> None:
            raise review.ReviewError("boom")

        results, failures = review.run_parallel({"a": lambda: 1, "b": fail, "c": lambda: 3})
        self.assertEqual((list(results), failures), (["a", "c"], {"b": "boom"}))


class ThreadTests(unittest.TestCase):
    def test_parse_threads_marks_bot_ownership(self) -> None:
        def node(thread_id: str, login: str, body: str) -> dict:
            return {"id": thread_id, "isResolved": False, "isOutdated": False, "path": "a", "line": 1,
                    "comments": {"nodes": [{"url": f"u/{thread_id}", "author": {"login": login},
                                            "body": body}]}}

        payload = {"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": [
            node("bot", "github-actions", "found a bug"),
            node("marked", "someone", f"{render.MARKER}\nbug"),
            node("human", "alice", "nit"),
            {**node("empty", "x", ""), "comments": {"nodes": []}},
        ]}}}}}
        threads = review.parse_threads(payload)
        self.assertEqual({t["thread_id"]: t["owned_by_bot"] for t in threads},
                         {"bot": True, "marked": True, "human": False})
        self.assertEqual(threads[0]["url"], "u/bot")


if __name__ == "__main__":
    unittest.main()
