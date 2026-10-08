#!/usr/bin/env python3
"""Unit tests for review.py and render.py. Run with `python3 .agents/skills/pr-review/test_review.py`."""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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


def thread(thread_id: str, *, resolved: bool = False, bot: bool = True, body: str = "A problem.",
           comment_id: int | None = 100) -> dict:
    return {"thread_id": thread_id, "url": f"https://example.test/{thread_id}", "resolved": resolved,
            "outdated": False, "path": "src/a.rs", "line": 2, "owned_by_bot": bot,
            "comment_id": comment_id, "first_body": body, "bot_resolved": bot and render.is_marked_resolved(body),
            "comments": [{"author": "github-actions", "body": body}]}


def resolved_by_bot(thread_id: str, original: str = "**Major:** One.") -> dict:
    """A thread the bot has already marked as resolved by editing its comment."""
    return thread(thread_id, body=render.mark_resolved(f"{render.MARKER}\n{original}", "fixed in `run`"))


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

    def test_a_finding_with_no_body_still_renders_everywhere(self) -> None:
        for body in ("", "   ", "\n\n"):
            with self.subTest(body=body):
                finding = render.Finding.from_action(comment(body=body))
                self.assertEqual(finding.markdown(), finding.header())
                self.assertEqual(finding.details(), "")
                text = render.render_summary(
                    overview=None, new=[], outside=[finding], threads=[], reopened=set(), fixed=set(),
                    failed=[], details="d", repo="base/base", head_sha="abc",
                    replace_existing=False)
                self.assertIn("### Outside the diff", text)

    def test_long_title_is_clipped(self) -> None:
        finding = render.Finding.from_action(comment(title="x" * 200))
        self.assertLessEqual(len(finding.title), render.MAX_TITLE_CHARS)

    def test_thread_header_round_trips(self) -> None:
        finding = render.Finding.from_action(comment(severity="critical", category="block-production"))
        body = f"{render.MARKER}\n{finding.markdown()}"
        severity, text = render.thread_header(thread("t", body=body))
        self.assertEqual((severity, text), ("critical", finding.header()))

    def test_thread_header_reads_the_first_versions_plain_label(self) -> None:
        body = f"{render.MARKER}\n**Major:** This overlay does not stop a PR. Second sentence."
        severity, text = render.thread_header(thread("t", body=body))
        self.assertEqual((severity, text), ("major", "🟠 **Major** — This overlay does not stop a PR."))

    def test_threads_with_empty_or_marker_only_bodies_render(self) -> None:
        for body in ("", render.MARKER, f"{render.MARKER}\n\n"):
            with self.subTest(body=body):
                self.assertEqual(render.thread_header(thread("t", body=body)), (None, "💬 (empty comment)"))
                text = render.render_summary(
                    overview=None, new=[], outside=[], threads=[thread("t", body=body)], reopened=set(),
                    fixed=set(), failed=[], details="d", repo="base/base", head_sha="abc",
                    replace_existing=False)
                self.assertIn("(empty comment)", text)

    def test_oversized_bodies_are_clipped_below_the_github_limit(self) -> None:
        finding = render.Finding.from_action(comment(body="x" * 100_000, evidence="y" * 100_000))
        self.assertLessEqual(len(finding.markdown()), render.MAX_BODY_CHARS)

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

    def test_reply_resolve_and_reopen(self) -> None:
        self.threads.append(resolved_by_bot("marked"))
        plan = self.plan({"type": "reply", "thread_id": "open", "body": "more"},
                         {"type": "resolve", "thread_id": "open", "body": "fixed in `run`"},
                         {"type": "reopen", "thread_id": "marked", "body": "still broken"})
        self.assertEqual([r["thread_id"] for r in plan.replies], ["open"])
        self.assertIn("**Follow-up:** more", plan.replies[0]["body"])
        [resolve] = plan.resolves
        self.assertEqual((resolve["thread_id"], resolve["comment_id"]), ("open", 100))
        self.assertTrue(resolve["body"].startswith(f"{render.MARKER}\n{render.RESOLVED_PREFIX}"))
        self.assertIn("A problem.", resolve["body"])
        [reopen] = plan.reopens
        self.assertIn("**Reopened:** still broken", reopen["body"])
        self.assertEqual(plan.rejected, [])

    def test_resolve_applies_to_an_open_bot_thread_only(self) -> None:
        self.threads.append(resolved_by_bot("marked"))
        plan = self.plan({"type": "resolve", "thread_id": "open", "body": "fixed"},
                         {"type": "resolve", "thread_id": "done", "body": "x"},
                         {"type": "resolve", "thread_id": "marked", "body": "x"},
                         {"type": "resolve", "thread_id": "human", "body": "x"})
        self.assertEqual([r["thread_id"] for r in plan.resolves], ["open"])
        self.assertEqual(len(plan.rejected), 3)

    def test_reopen_applies_to_a_thread_the_bot_marked_resolved_only(self) -> None:
        self.threads.append(resolved_by_bot("marked"))
        plan = self.plan({"type": "reopen", "thread_id": "open", "body": "x"},
                         {"type": "reopen", "thread_id": "done", "body": "x"},
                         {"type": "reopen", "thread_id": "human", "body": "x"})
        self.assertEqual(plan.reopens, [])
        self.assertEqual(len(plan.rejected), 3)

    def test_a_thread_the_bot_marked_and_a_person_then_closed_is_not_reopened(self) -> None:
        # The person closing it on GitHub is the final word; a new comment is the way to raise it again.
        closed = {**resolved_by_bot("closed"), "resolved": True}
        self.threads.append(closed)
        plan = self.plan({"type": "reopen", "thread_id": "closed", "body": "it is back"})
        self.assertEqual((plan.reopens, len(plan.rejected)), ([], 1))

    def test_a_thread_gets_one_change_per_run(self) -> None:
        plan = self.plan({"type": "resolve", "thread_id": "open", "body": "a"},
                         {"type": "resolve", "thread_id": "open", "body": "b"})
        self.assertEqual(len(plan.resolves), 1)
        self.assertEqual(len(plan.rejected), 1)

    def test_a_thread_with_no_comment_id_cannot_be_resolved(self) -> None:
        self.threads.append(thread("noid", comment_id=None))
        plan = self.plan({"type": "resolve", "thread_id": "noid", "body": "x"})
        self.assertEqual((plan.resolves, len(plan.rejected)), ([], 1))

    def test_invalid_thread_actions_are_rejected(self) -> None:
        plan = self.plan({"type": "reply", "thread_id": "human", "body": "x"},
                         {"type": "reply", "thread_id": "missing", "body": "x"},
                         {"type": "reopen", "thread_id": "open", "body": "x"})
        self.assertEqual((plan.replies, plan.reopens), ([], []))
        self.assertEqual(len(plan.rejected), 3)


class MarkResolvedTests(unittest.TestCase):
    ORIGINAL = f"{render.MARKER}\n🟠 **Major · Safety** — A title\n\nThe explanation.\n\n**Suggested fix:** Do x."

    def test_marking_keeps_the_original_and_says_what_happened(self) -> None:
        marked = render.mark_resolved(self.ORIGINAL, "fixed in `run`")
        self.assertTrue(render.is_marked_resolved(marked))
        self.assertIn("fixed in `run`", marked.splitlines()[1])
        self.assertIn("<summary>Original comment</summary>", marked)
        self.assertIn("The explanation.", marked)

    def test_unmarking_restores_the_original_exactly(self) -> None:
        marked = render.mark_resolved(self.ORIGINAL, "fixed")
        self.assertEqual(render.unmark_resolved(marked), self.ORIGINAL)

    def test_marking_twice_and_unmarking_twice_is_stable(self) -> None:
        once = render.mark_resolved(self.ORIGINAL, "fixed")
        self.assertEqual(render.unmark_resolved(render.mark_resolved(render.unmark_resolved(once), "fixed")),
                         self.ORIGINAL)

    def test_a_plain_comment_is_not_marked_and_cannot_be_unmarked(self) -> None:
        self.assertFalse(render.is_marked_resolved(self.ORIGINAL))
        self.assertIsNone(render.unmark_resolved(self.ORIGINAL))

    def test_a_marked_thread_is_described_by_the_original_problem(self) -> None:
        marked = render.mark_resolved(self.ORIGINAL, "fixed")
        severity, text = render.thread_header({"comments": [{"body": marked}]})
        self.assertEqual(severity, "major")
        self.assertEqual(text, "🟠 **Major · Safety** — A title")

    def test_a_marked_legacy_comment_is_described_by_the_original_problem(self) -> None:
        marked = render.mark_resolved(f"{render.MARKER}\n**Minor:** Old style. More.", "fixed")
        self.assertEqual(render.thread_header({"comments": [{"body": marked}]}),
                         ("minor", "🟡 **Minor** — Old style."))

    def test_a_marked_comment_with_a_damaged_original_still_renders(self) -> None:
        damaged = f"{render.MARKER}\n{render.RESOLVED_PREFIX} — fixed\n\nno details block here"
        severity, text = render.thread_header({"comments": [{"body": damaged}]})
        self.assertIsNone(severity)
        self.assertIn("Resolved by the bot", text)

    def test_a_long_reason_is_clipped(self) -> None:
        marked = render.mark_resolved(self.ORIGINAL, "word " * 400)
        self.assertLess(len(marked.splitlines()[1]), 450)


class SummaryTests(unittest.TestCase):
    def summary(self, plan: review.Plan, threads: list[dict] | None = None, *, failed: list[str] | None = None,
                replace_existing: bool = False) -> str | None:
        return render.render_summary(
            overview="One panic path.", new=plan.new, outside=plan.outside, threads=threads or [],
            reopened={u["thread_id"] for u in plan.reopens}, fixed={r["thread_id"] for r in plan.resolves},
            failed=failed or [], details="Details.",
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
        marked = resolved_by_bot("marked")
        plan = review.build_plan({"actions": [{"type": "reopen", "thread_id": "marked", "body": "again"}]},
                                 [marked], set())
        text = self.summary(plan, [marked])
        self.assertIn("### Reopened", text)
        self.assertNotIn("Open from earlier reviews", text)

    def test_resolved_threads_are_listed_as_fixed_and_not_counted(self) -> None:
        threads = [thread("a", body=f"{render.MARKER}\n**Major:** One."), thread("b")]
        plan = review.build_plan({"actions": [{"type": "resolve", "thread_id": "a", "body": "gone"}]},
                                 threads, set())
        text = self.summary(plan, threads)
        self.assertIn("### Fixed in this push", text)
        self.assertIn("- ✅ 🟠 **Major** — One.", text)
        self.assertIn("## 💬 1 open", text)

    def test_a_thread_the_bot_marked_resolved_earlier_is_done(self) -> None:
        text = self.summary(review.Plan(), [resolved_by_bot("marked")], replace_existing=True)
        self.assertIn("## ✅ No open findings", text)
        self.assertNotIn("Open from earlier reviews", text)

    def test_open_outdated_threads_stay_in_the_summary(self) -> None:
        outdated = {**thread("old", body=f"{render.MARKER}\n**Major:** Still broken."), "outdated": True}
        text = self.summary(review.Plan(), [outdated])
        self.assertIn("## 🟠 1 major", text)
        self.assertIn("### Open from earlier reviews", text)

    def test_summary_is_capped(self) -> None:
        plan = review.build_plan({"actions": [comment(line=99, body="z" * 5000, title=f"t{i}")
                                              for i in range(40)]}, [], review.diff_new_lines(DIFF))
        self.assertLessEqual(len(self.summary(plan)), render.MAX_BODY_CHARS + 1)

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
    @staticmethod
    def node(thread_id: str, login: str, body: str, typename: str = "Bot", extra: list | None = None) -> dict:
        def c(url: str, who: str, text: str, kind: str) -> dict:
            return {"databaseId": 7, "url": url, "body": text, "author": {"login": who, "__typename": kind}}

        first = c(f"u/{thread_id}", login, body, typename)
        return {"id": thread_id, "isResolved": False, "isOutdated": False, "path": "a", "line": 1,
                "root": {"nodes": [first]}, "recent": {"nodes": [first] + (extra or [])}}

    def page(self, *nodes: dict) -> dict:
        return {"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": list(nodes)}}}}}

    def test_ownership_is_decided_by_author_alone(self) -> None:
        threads = review.parse_threads([self.page(
            self.node("bot", "github-actions", "found a bug"),
            self.node("quoted", "alice", f"{render.MARKER}\nquoting the bot", typename="User"),
            self.node("lookalike", "github-actions", "x", typename="User"),
            self.node("human", "alice", "nit", typename="User"),
            {**self.node("empty", "x", ""), "root": {"nodes": []}, "recent": {"nodes": []}},
        )])
        self.assertEqual({t["thread_id"]: t["owned_by_bot"] for t in threads},
                         {"bot": True, "quoted": False, "lookalike": False, "human": False})
        self.assertEqual(threads[0]["url"], "u/bot")

    def test_the_first_comment_decides_whether_the_bot_marked_the_thread_resolved(self) -> None:
        marked = render.mark_resolved(f"{render.MARKER}\n**Major:** One.", "fixed")
        threads = review.parse_threads([self.page(
            self.node("marked", "github-actions", marked),
            self.node("plain", "github-actions", "**Major:** One."),
            self.node("faked", "alice", marked, typename="User"))])
        self.assertEqual({t["thread_id"]: t["bot_resolved"] for t in threads},
                         {"marked": True, "plain": False, "faked": False})
        self.assertEqual((threads[0]["comment_id"], threads[0]["first_body"]), (7, marked))

    def test_pages_are_concatenated(self) -> None:
        threads = review.parse_threads([self.page(self.node("a", "github-actions", "x")),
                                        self.page(self.node("b", "github-actions", "y"))])
        self.assertEqual([t["thread_id"] for t in threads], ["a", "b"])

    def test_first_comment_is_kept_when_the_recent_window_omits_it(self) -> None:
        reply = {"url": "u/reply", "body": "fixed", "author": {"login": "alice", "__typename": "User"}}
        node = self.node("a", "github-actions", "root", extra=[reply])
        node["recent"]["nodes"] = [reply]
        [thread] = review.parse_threads([self.page(node)])
        self.assertEqual([c["body"] for c in thread["comments"]], ["root", "fixed"])

    def test_diff_from_files_rebuilds_a_diff_that_anchors_comments(self) -> None:
        files = [[{"filename": "src/a.rs", "status": "modified", "patch": "@@ -1,2 +1,3 @@\n keep\n+new\n tail"},
                  {"filename": "b.bin", "status": "added"},
                  {"filename": "new.rs", "previous_filename": "old.rs", "status": "renamed",
                   "patch": "@@ -1 +1 @@\n-x\n+y"}]]
        with mock.patch.object(review, "gh", return_value=json.dumps(files)):
            diff = review.diff_from_files(1, "base/base")
        self.assertEqual(review.diff_new_lines(diff), {("src/a.rs", 1), ("src/a.rs", 2), ("src/a.rs", 3),
                                                       ("new.rs", 1)})
        self.assertIn("diff --git a/old.rs b/new.rs", diff)


class PrContextTests(unittest.TestCase):
    """pr_context against a scripted gh."""

    HEAD = "a" * 40

    def scripted(self, *, fail: tuple[str, ...] = (), head: str | None = None, files_pages: list | None = None):
        calls: list[str] = []

        def fake_gh(args, input_text=None):
            joined = " ".join(args)
            calls.append(joined)
            for needle in fail:
                if needle in joined:
                    raise review.ReviewError(f"boom: {needle}")
            if args[:2] == ["pr", "view"]:
                return json.dumps({"title": "T", "body": "B", "headRefOid": head or self.HEAD,
                                   "files": [{"path": "src/a.rs"}]})
            if args[:2] == ["pr", "diff"]:
                return DIFF
            if "pulls/7/files" in joined:
                return json.dumps(files_pages or [[{"filename": "src/a.rs", "status": "modified",
                                                    "patch": "@@ -1 +1,2 @@\n keep\n+new"}]])
            if "reviewThreads" in joined:
                return json.dumps([{"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": []}}}}}])
            return ""

        return fake_gh, calls

    def context(self, gh_fn, checked_out: str | None = None, post: bool = False):
        with mock.patch.object(review, "gh", gh_fn), mock.patch.object(
                review, "git", return_value=(checked_out or self.HEAD) + "\n"):
            return review.pr_context(7, "base/base", post=post)

    def test_the_diff_is_rebuilt_from_the_files_api_when_gh_pr_diff_fails(self) -> None:
        gh_fn, calls = self.scripted(fail=("pr diff",))
        ctx = self.context(gh_fn)
        self.assertIn(("src/a.rs", 2), review.diff_new_lines(ctx.diff))
        files_call = next(c for c in calls if "pulls/7/files" in c)
        self.assertIn("--paginate", files_call)
        self.assertIn("--slurp", files_call)

    def test_all_pages_of_the_files_api_are_used(self) -> None:
        pages = [[{"filename": "a.rs", "status": "modified", "patch": "@@ -1 +1 @@\n+x"}],
                 [{"filename": "b.rs", "status": "modified", "patch": "@@ -1 +1 @@\n+y"}]]
        gh_fn, _ = self.scripted(fail=("pr diff",), files_pages=pages)
        anchors = {p for p, _ in review.diff_new_lines(self.context(gh_fn).diff)}
        self.assertEqual(anchors, {"a.rs", "b.rs"})

    def test_threads_are_read_with_pagination(self) -> None:
        gh_fn, calls = self.scripted()
        self.context(gh_fn)
        call = next(c for c in calls if "reviewThreads" in c)
        self.assertIn("--paginate", call)
        self.assertIn("--slurp", call)

    def test_a_failed_previous_summary_lookup_does_not_stop_the_review(self) -> None:
        gh_fn, _ = self.scripted(fail=("startswith",))
        self.assertIsNone(self.context(gh_fn).previous_summary)

    def test_posting_from_the_wrong_commit_is_refused(self) -> None:
        gh_fn, _ = self.scripted()
        with self.assertRaises(review.ReviewError):
            self.context(gh_fn, checked_out="b" * 40, post=True)

    def test_a_local_run_on_another_commit_only_warns(self) -> None:
        gh_fn, _ = self.scripted()
        self.assertEqual(self.context(gh_fn, checked_out="b" * 40, post=False).head_sha, self.HEAD)


class BudgetTests(unittest.TestCase):
    def agent(self, timeout: int = 1000) -> review.Agent:
        return review.Agent("a", "review", ("always",), "m", "high", "Read", timeout, None, None, "p",
                            Path("a.md"))

    def test_timeout_shrinks_to_leave_time_for_later_stages(self) -> None:
        budget = review.Budget(2000)
        self.assertEqual(budget.timeout_for(1000, "a"), 1000)
        self.assertLessEqual(budget.timeout_for(1000, "a", reserve=1500), 500)

    def test_stage_is_skipped_when_the_budget_is_spent(self) -> None:
        with self.assertRaises(review.ReviewError):
            review.Budget(100).timeout_for(1000, "a", reserve=90)

    def test_the_decider_always_gets_its_full_time_even_if_every_stage_uses_all_of_its_own(self) -> None:
        agents = {a.name: a for a in review.load_agents()}
        members = [a for a in agents.values() if a.stage == "council"]
        decide, chair = agents["decide"], agents["council-chair"]
        now = [0.0]
        with mock.patch.object(review.time, "monotonic", lambda: now[0]):
            budget = review.Budget(review.DEFAULT_BUDGET_SECONDS)

            def spend(wanted: int, reserve: float) -> None:
                now[0] += budget.timeout_for(wanted, "stage", reserve)

            spend(agents["triage"].timeout_seconds, decide.timeout_seconds)
            spend(max(m.timeout_seconds for m in members),
                  decide.timeout_seconds + review.VOTE_TIMEOUT_SECONDS + chair.timeout_seconds)
            spend(review.VOTE_TIMEOUT_SECONDS, decide.timeout_seconds + chair.timeout_seconds)
            spend(chair.timeout_seconds, decide.timeout_seconds)
            # Every stage ran as long as it was allowed to; the decider must still fit in full.
            self.assertEqual(budget.timeout_for(decide.timeout_seconds, "decide"), decide.timeout_seconds)
            now[0] += decide.timeout_seconds
            # And the whole run ends with time to spare in the CI job.
            self.assertLessEqual(now[0], 90 * 60 - 600)

    def test_the_default_budget_leaves_the_ci_job_time_to_post(self) -> None:
        self.assertLessEqual(review.DEFAULT_BUDGET_SECONDS, 90 * 60 - 600)

    def test_a_stage_that_ate_the_reserve_is_skipped_not_run_over(self) -> None:
        with mock.patch.object(review.time, "monotonic", lambda: 0.0):
            budget = review.Budget(1000)
            self.assertLessEqual(budget.timeout_for(5000, "a", reserve=900), 100)
            with self.assertRaises(review.ReviewError):
                budget.timeout_for(5000, "a", reserve=990)

    def test_gh_calls_are_bounded(self) -> None:
        with mock.patch.object(review, "run", return_value="") as run:
            review.gh(["api", "x"])
            review.git(["status"])
        self.assertEqual([c.kwargs["timeout"] for c in run.call_args_list],
                         [review.GH_TIMEOUT_SECONDS, review.GH_TIMEOUT_SECONDS])

    def test_agents_do_not_inherit_github_credentials(self) -> None:
        with mock.patch.dict(os.environ, {"GH_TOKEN": "x", "GITHUB_TOKEN": "y", "ANTHROPIC_API_KEY": "z"}):
            env = review.agent_env()
        self.assertEqual((env.get("GH_TOKEN"), env.get("GITHUB_TOKEN"), env.get("ANTHROPIC_API_KEY")),
                         (None, None, "z"))


class AgentRunTests(unittest.TestCase):
    def agent(self, **overrides) -> review.Agent:
        fields = dict(name="a", stage="review", when=("always",), model="m", effort="high", tools="Read",
                      timeout_seconds=100, max_budget_usd=None, max_output_tokens=None, prompt="p",
                      path=Path("a.md"))
        return review.Agent(**{**fields, **overrides})

    def envelope(self, **extra) -> str:
        return json.dumps({"is_error": False, "structured_output": {"findings": []}, "total_cost_usd": 0.1,
                           "modelUsage": {"resolved-model": {}}, **extra})

    def call(self, agent: review.Agent, runs: list):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(review, "run", side_effect=runs) as run:
            result = review.run_agent(agent, "prompt", Path(tmp), Path(tmp), None)
        return result, run

    def test_max_output_tokens_is_passed_through_settings_only_when_set(self) -> None:
        _, run = self.call(self.agent(max_output_tokens=32000), [self.envelope()])
        cmd = run.call_args.args[0]
        settings = json.loads(cmd[cmd.index("--settings") + 1])
        self.assertEqual(settings, {"env": {"CLAUDE_CODE_MAX_OUTPUT_TOKENS": "32000"}})
        _, run = self.call(self.agent(), [self.envelope()])
        self.assertNotIn("--settings", run.call_args.args[0])

    def test_pr_settings_are_never_loaded(self) -> None:
        _, run = self.call(self.agent(), [self.envelope()])
        cmd = run.call_args.args[0]
        self.assertEqual(cmd[cmd.index("--setting-sources") + 1], "user")

    def test_the_model_that_really_ran_is_recorded(self) -> None:
        review.MODELS_RAN.clear()
        self.call(self.agent(name="alias-agent", model="opus"), [self.envelope()])
        self.assertEqual(review.MODELS_RAN["alias-agent"], "resolved-model")

    def test_a_retry_gets_only_the_time_that_is_left(self) -> None:
        empty = json.dumps({"is_error": False, "structured_output": None})
        now = [0.0]
        timeouts: list[int] = []

        def slow_first(cmd, **kwargs):
            timeouts.append(kwargs["timeout"])
            now[0] += 800  # the first attempt uses almost all of its time and returns nothing
            return empty if len(timeouts) == 1 else self.envelope()

        with mock.patch.object(review.time, "monotonic", lambda: now[0]):
            budget = review.Budget(1000)
            with tempfile.TemporaryDirectory() as tmp, mock.patch.object(review, "run", side_effect=slow_first):
                review.run_agent(self.agent(timeout_seconds=900), "p", Path(tmp), Path(tmp), None,
                                 budget=budget, reserve=0)
        self.assertEqual(timeouts[0], 900)
        self.assertLessEqual(timeouts[1], 200)

    def test_no_retry_when_the_budget_is_spent(self) -> None:
        empty = json.dumps({"is_error": False, "structured_output": None})
        now = [0.0]

        def use_it_all(cmd, **kwargs):
            now[0] += 990
            return empty

        with mock.patch.object(review.time, "monotonic", lambda: now[0]):
            budget = review.Budget(1000)
            with tempfile.TemporaryDirectory() as tmp, mock.patch.object(
                    review, "run", side_effect=use_it_all) as run, self.assertRaises(review.ReviewError):
                review.run_agent(self.agent(timeout_seconds=900), "p", Path(tmp), Path(tmp), None,
                                 budget=budget, reserve=0)
        self.assertEqual(run.call_count, 1)

    def test_an_empty_answer_is_retried_once(self) -> None:
        empty = json.dumps({"is_error": False, "structured_output": None, "subtype": "success", "result": ""})
        result, run = self.call(self.agent(), [empty, self.envelope()])
        self.assertEqual(result, {"findings": []})
        self.assertEqual(run.call_count, review.AGENT_ATTEMPTS)

    def test_a_second_failure_is_raised(self) -> None:
        empty = json.dumps({"is_error": False, "structured_output": None})
        with self.assertRaises(review.ReviewError):
            self.call(self.agent(), [empty, empty])

    def test_failures_that_a_retry_cannot_fix_are_not_retried(self) -> None:
        for reason in ("`claude` timed out after 5s", "API Error: 403 Access denied to restricted model",
                       "Invalid model name passed in model=x"):
            with self.subTest(reason=reason), self.assertRaises(review.ReviewError):
                self.call(self.agent(), [review.ReviewError(reason), self.envelope()])

    def test_a_failed_command_keeps_its_full_output(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "x.failed.txt"
            with self.assertRaises(review.ReviewError):
                review.run([sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr); "
                            "sys.exit(3)"], failure_log=log)
            text = log.read_text()
        self.assertIn("exit 3", text)
        self.assertIn("out", text)
        self.assertIn("err", text)

    def test_no_file_in_the_skill_shadows_a_standard_library_module(self) -> None:
        # The script's directory comes first on sys.path, so a file named like a stdlib module would
        # replace it for every import in a process that holds credentials.
        names = {p.stem for p in review.SKILL_DIR.glob("*.py")} | {
            p.name for p in review.SKILL_DIR.iterdir() if p.is_dir()}
        self.assertEqual(sorted(names & set(sys.stdlib_module_names)), [])

    def test_max_output_tokens_is_read_from_the_agent_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.md"
            path.write_text("---\nstage: review\nmodel: m\nmax_output_tokens: 32000\n---\nx")
            self.assertEqual(review.parse_agent(path).max_output_tokens, 32000)
        self.assertTrue(any(a.max_output_tokens for a in review.load_agents()), "a Gemini member needs one")


class FailureReasonTests(unittest.TestCase):
    def result(self, stdout: str = "", stderr: str = ""):
        return mock.Mock(stdout=stdout, stderr=stderr)

    def test_the_api_error_in_stdout_is_reported_not_the_model_warning(self) -> None:
        reason = review.describe_failure(self.result(
            stdout=json.dumps({"is_error": True, "result": "API Error: 403 Access denied to restricted model"}),
            stderr='[claude-code:unrecognized_model] {"model":"grok-4.7"}'))
        self.assertIn("403", reason)
        self.assertNotIn("unrecognized_model", reason)

    def test_plain_stderr_is_used_when_stdout_is_not_json(self) -> None:
        self.assertEqual(review.describe_failure(self.result(stdout="", stderr="gh: HTTP 502")), "gh: HTTP 502")

    def test_the_warning_alone_is_better_than_nothing(self) -> None:
        reason = review.describe_failure(self.result(stderr="[claude-code:unrecognized_model] x"))
        self.assertTrue(reason)


class FakeGh:
    """A stand-in for `gh` that records calls and fails the ones a test names."""

    def __init__(self, fail: tuple[str, ...] = (), responses: dict[str, str] | None = None) -> None:
        self.calls: list[list[str]] = []
        self.inputs: list[str | None] = []
        self.fail = fail
        self.responses = responses or {}

    def __call__(self, args: list[str], input_text: str | None = None) -> str:
        self.calls.append(args)
        self.inputs.append(input_text)
        joined = " ".join(args)
        for needle in self.fail:
            if needle in joined:
                raise review.ReviewError(f"boom: {needle}")
        return next((v for k, v in self.responses.items() if k in joined), "")

    def matching(self, needle: str) -> list[list[str]]:
        return [c for c in self.calls if needle in " ".join(c)]


class ApplyPlanTests(unittest.TestCase):
    def setUp(self) -> None:
        self.threads = [thread("open", comment_id=11), thread("fixed", comment_id=22),
                        resolved_by_bot("marked"), thread("done", resolved=True)]
        self.ctx = review.Context(description="d", title="t", files=["src/a.rs"], diff=DIFF, pr_number=7,
                                  head_sha="abc", threads=self.threads)
        decision = {"actions": [
            comment(), {"type": "reply", "thread_id": "open", "body": "more"},
            {"type": "resolve", "thread_id": "fixed", "body": "fixed in `run`"},
            {"type": "reopen", "thread_id": "marked", "body": "still broken"}]}
        self.plan = review.build_plan(decision, self.ctx.threads, review.diff_new_lines(DIFF))

    def apply(self, gh: FakeGh) -> list[str]:
        def summarize(plan: review.Plan) -> str | None:
            return render.render_summary(
                overview=None, new=plan.new, outside=plan.outside, threads=self.ctx.threads,
                reopened={u["thread_id"] for u in plan.reopens}, fixed={r["thread_id"] for r in plan.resolves},
                failed=[], details="d", repo="base/base", head_sha="abc", replace_existing=True)

        gh.responses.setdefault("pr view", json.dumps({"headRefOid": "abc"}))
        with mock.patch.object(review, "gh", gh):
            return review.apply_plan(self.plan, self.ctx, summarize)

    def test_everything_is_posted_and_the_old_summary_is_replaced_after_the_new_one(self) -> None:
        gh = FakeGh(responses={"issues/7/comments": "11\n12\n"})
        self.assertEqual(self.apply(gh), [])
        self.assertEqual(len(gh.matching("pulls/7/reviews")), 1)
        comment_calls = [i for i, c in enumerate(gh.calls) if c[:2] == ["pr", "comment"]]
        deletes = [i for i, c in enumerate(gh.calls) if "DELETE" in c]
        self.assertEqual(len(comment_calls), 1)
        self.assertEqual(len(deletes), 2)
        self.assertLess(comment_calls[0], min(deletes))

    def test_resolving_edits_the_bots_own_comment_and_never_calls_the_resolve_mutation(self) -> None:
        gh = FakeGh()
        self.apply(gh)
        self.assertEqual(gh.matching("resolveReviewThread"), [])
        [edit] = [(c, i) for c, i in zip(gh.calls, gh.inputs, strict=True)
                  if "PATCH" in c and "pulls/comments/22" in " ".join(c)]
        body = json.loads(edit[1])["body"]
        self.assertTrue(body.startswith(f"{render.MARKER}\n{render.RESOLVED_PREFIX}"))
        self.assertIn("fixed in `run`", body)

    def test_reopening_puts_the_comment_back_and_replies(self) -> None:
        gh = FakeGh()
        self.apply(gh)
        [edit] = [i for c, i in zip(gh.calls, gh.inputs, strict=True) if "PATCH" in c and "comments/100" in " ".join(c)]
        self.assertFalse(render.is_marked_resolved(json.loads(edit)["body"]))
        self.assertTrue([c for c in gh.matching("addPullRequestReviewThreadReply") if "Reopened" in " ".join(c)])

    def test_a_failed_edit_is_not_reported_as_done(self) -> None:
        gh = FakeGh(fail=("PATCH",))
        problems = self.apply(gh)
        self.assertEqual(len(problems), 2)
        self.assertEqual((self.plan.resolves, self.plan.reopens), ([], []))
        [summary] = [i for c, i in zip(gh.calls, gh.inputs, strict=True) if c[:2] == ["pr", "comment"]]
        self.assertNotIn("### Fixed in this push", summary)
        self.assertNotIn("### Reopened", summary)
        # No reply claims the thread was reopened when the comment was not put back.
        self.assertFalse([c for c in gh.matching("addPullRequestReviewThreadReply") if "Reopened" in " ".join(c)])

    def test_a_dirty_tree_is_refused_when_posting_but_not_locally(self) -> None:
        def fake_git(args):
            return " M review.py\n" if args[0] == "status" else "a" * 40 + "\n"

        for post, refused in ((True, True), (False, False)):
            with self.subTest(post=post):
                gh_fn, _ = PrContextTests().scripted()
                with mock.patch.object(review, "gh", gh_fn), mock.patch.object(review, "git", fake_git):
                    if refused:
                        with self.assertRaises(review.ReviewError):
                            review.pr_context(7, "base/base", post=post)
                    else:
                        review.pr_context(7, "base/base", post=post)

    def test_nothing_is_posted_if_the_pull_request_moved_on(self) -> None:
        gh = FakeGh(responses={"pr view": json.dumps({"headRefOid": "newer"})})
        with self.assertRaises(review.ReviewError):
            self.apply(gh)
        self.assertEqual([c for c in gh.calls if c[0] != "pr"], [])

    def test_summary_query_only_matches_the_bot(self) -> None:
        gh = FakeGh()
        self.apply(gh)
        for call in gh.matching("startswith"):
            self.assertIn('.user.login == "github-actions[bot]"', " ".join(call))
            self.assertIn('.user.type == "Bot"', " ".join(call))

    def test_a_rejected_inline_review_moves_findings_into_the_summary(self) -> None:
        gh = FakeGh(fail=("pulls/7/reviews",))
        problems = self.apply(gh)
        self.assertEqual(len(problems), 1)
        self.assertEqual((self.plan.new, len(self.plan.outside)), ([], 1))
        [summary] = [i for c, i in zip(gh.calls, gh.inputs, strict=True) if c[:2] == ["pr", "comment"]]
        self.assertIn("### Outside the diff", summary)
        self.assertIn("Panics on empty batch", summary)

    def test_the_summary_lists_the_threads_that_were_changed(self) -> None:
        gh = FakeGh()
        self.apply(gh)
        [summary] = [i for c, i in zip(gh.calls, gh.inputs, strict=True) if c[:2] == ["pr", "comment"]]
        self.assertIn("### Fixed in this push", summary)
        self.assertIn("### Reopened", summary)

    def test_failing_to_list_old_summaries_still_posts_the_new_one(self) -> None:
        gh = FakeGh(fail=("startswith",))
        problems = self.apply(gh)
        self.assertEqual(len(problems), 1)
        self.assertEqual(len([c for c in gh.calls if c[:2] == ["pr", "comment"]]), 1)

    def test_a_failed_reply_does_not_stop_the_summary(self) -> None:
        gh = FakeGh(fail=("addPullRequestReviewThreadReply",))
        problems = self.apply(gh)
        # The follow-up on one thread and the reply that goes with reopening another.
        self.assertEqual(len(problems), 2)
        self.assertEqual(self.plan.reopens, [])
        self.assertEqual(len(gh.matching("pulls/7/reviews")), 1)
        self.assertEqual(len([c for c in gh.calls if c[:2] == ["pr", "comment"]]), 1)

    def test_a_failed_summary_post_keeps_the_old_summary_and_is_reported(self) -> None:
        gh = FakeGh(fail=("pr comment",), responses={"issues/7/comments": "11\n"})
        problems = self.apply(gh)
        self.assertEqual(len(problems), 1)
        self.assertEqual(gh.matching("DELETE"), [])
        # Everything else was still posted, so the caller can go on to write the handoff file.
        self.assertEqual(len(gh.matching("pulls/7/reviews")), 1)

    def test_the_review_payload_is_what_github_requires(self) -> None:
        gh = FakeGh()
        self.apply(gh)
        [payload] = [json.loads(i) for c, i in zip(gh.calls, gh.inputs, strict=True) if "pulls/7/reviews" in " ".join(c)]
        self.assertEqual((payload["event"], payload["commit_id"]), ("COMMENT", "abc"))
        [item] = payload["comments"]
        self.assertEqual((item["path"], item["line"], item["side"]), ("src/a.rs", 3, "RIGHT"))
        self.assertTrue(item["body"].startswith(render.MARKER))


class PipelineFailureTests(unittest.TestCase):
    """run_pipeline must still produce a decision when individual agents fail."""

    def setUp(self) -> None:
        self.ctx = review.Context(description="d", title="t", files=["src/a.rs"], diff=DIFF)
        self.agents = review.load_agents()
        self.calls: list[str] = []
        self.failing: set[str] = set()
        self.triage = {"depth": "deep", "block_production_sensitive": False, "reasoning": "r", "focus_areas": []}
        self.finding = {"title": "t", "severity": "major", "category": "safety", "confidence": "high",
                        "path": "src/a.rs", "line": 1, "explanation": "e"}

    def fake_run_agent(self, agent, prompt, cwd, artifacts, model_override, *, schema=None, label=None,
                       **_: object) -> dict:
        label = label or agent.name
        self.calls.append(label)
        if label in self.failing:
            raise review.ReviewError(f"{label} broke")
        if agent.stage == "triage":
            return self.triage
        if schema == "votes":
            return {"votes": []}
        if agent.stage == "chair":
            return {"findings": []}
        if agent.stage == "decide":
            return {"actions": [], "overview": None, "dropped": []}
        return {"findings": [self.finding] if agent.name == "council-adversary" else []}

    def run_pipeline(self) -> review.Outcome:
        with mock.patch.object(review, "run_agent", self.fake_run_agent):
            return review.run_pipeline(self.ctx, self.agents, Path("."), Path("."), None, review.Budget(10_000))

    def test_deep_change_runs_council_votes_chair_and_decide(self) -> None:
        outcome = self.run_pipeline()
        self.assertEqual(outcome.failed, {})
        self.assertIn("council-chair", self.calls)
        self.assertIn("council-invariants.vote", self.calls)
        self.assertEqual(self.calls[-1], "decide")

    def test_standard_change_skips_the_council(self) -> None:
        self.triage["depth"] = "standard"
        self.run_pipeline()
        self.assertFalse([c for c in self.calls if c.startswith("council")])

    def test_triage_failure_runs_everything_and_says_so(self) -> None:
        self.failing = {"triage"}
        outcome = self.run_pipeline()
        self.assertTrue(outcome.triage["block_production_sensitive"])
        self.assertIn("Triage failed", outcome.triage["reasoning"])
        self.assertIn("review-block-production", self.calls)
        self.assertIn("council-chair", self.calls)

    def test_chair_failure_passes_the_unmerged_findings_on(self) -> None:
        self.failing = {"council-chair"}
        outcome = self.run_pipeline()
        self.assertIn("council/council-chair", outcome.failed)
        [finding] = outcome.reviews["council"]["findings"]
        self.assertNotIn("id", finding)
        self.assertNotIn("reported_by", finding)
        self.assertIn("council-adversary", finding["support"])

    def test_one_member_failing_does_not_stop_the_council(self) -> None:
        self.failing = {"council-design"}
        outcome = self.run_pipeline()
        self.assertIn("council/council-design", outcome.failed)
        self.assertIn("council-chair", self.calls)

    def test_a_failed_decider_still_yields_a_summary_that_says_so(self) -> None:
        self.failing = {"decide"}
        outcome = self.run_pipeline()
        self.assertIn("decide", outcome.failed)
        plan = review.build_plan(outcome.decision, [], review.diff_new_lines(DIFF))
        self.assertEqual(plan.new, [])
        self.assertIn("final step failed", outcome.decision["overview"])

    def test_every_reviewer_failing_is_an_error(self) -> None:
        self.failing = {"review-general", "council-invariants", "council-adversary", "council-design"}
        with self.assertRaises(review.ReviewError):
            self.run_pipeline()


if __name__ == "__main__":
    unittest.main()
