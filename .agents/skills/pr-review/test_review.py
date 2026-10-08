#!/usr/bin/env python3
"""Unit tests for review.py and render.py. Run with `python3 .agents/skills/pr-review/test_review.py`."""

import dataclasses
import json
import os
import sys
import tempfile
import threading
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


def dataclasses_replace(agent, **changes):
    return dataclasses.replace(agent, **changes)


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
                    )
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
                    )
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
        plan = self.plan(comment(severity="minor", line=1, title="a"), comment(severity="critical", line=4, title="b"),
                         comment(severity="major", line=2, title="c"), comment(severity="major", line=1, title="d"))
        self.assertEqual([(f.severity, f.line) for f in plan.new],
                         [("critical", 4), ("major", 1), ("major", 2), ("minor", 1)])

    def test_inline_comments_are_capped_keeping_the_most_severe(self) -> None:
        actions = [comment(severity="minor", line=1, title=f"minor {i}") for i in range(review.MAX_INLINE_COMMENTS)]
        actions.append(comment(severity="critical", line=2, title="critical"))
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

    def test_a_marked_comment_without_its_original_cannot_be_reopened(self) -> None:
        damaged = thread("damaged", body=f"{render.MARKER}\n{render.RESOLVED_PREFIX} — fixed\n\nno details")
        self.threads.append(damaged)
        plan = self.plan({"type": "reopen", "thread_id": "damaged", "body": "back"})
        self.assertEqual((plan.reopens, len(plan.rejected)), ([], 1))
        self.assertIn("no original", plan.rejected[0])
        self.assertEqual(plan.back, ["damaged"])

    def test_the_decider_is_told_when_a_marked_thread_cannot_be_reopened(self) -> None:
        damaged = thread("damaged", body=f"{render.MARKER}\n{render.RESOLVED_PREFIX} — fixed\n\nno details")
        ctx = review.Context(description="d", title="t", files=[], diff=DIFF,
                             threads=[resolved_by_bot("marked"), damaged, thread("open")])
        prompt = review.decide_prompt(ctx, {}, {}, {})
        statuses = {t["thread_id"]: t["status"] for t in json.loads(
            prompt.split("<existing_threads>")[1].split("</existing_threads>")[0])}
        self.assertEqual(statuses["marked"], "marked resolved by the bot")
        self.assertIn("cannot be reopened", statuses["damaged"])
        self.assertEqual(statuses["open"], "open")

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
                status: str | None = None) -> str:
        return render.render_summary(
            overview="One panic path.", new=plan.new, outside=plan.outside, threads=threads or [],
            reopened={u["thread_id"] for u in plan.reopens}, fixed={r["thread_id"] for r in plan.resolves},
            failed=failed or [], details="Details.",
            repo="base/base", head_sha="abc123", status=status)

    def test_summary_lists_findings_with_links(self) -> None:
        plan = review.build_plan({"actions": [comment(severity="critical", title="Panics on empty batch"),
                                              comment(line=1, title="Leaks a handle")]}, [],
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
        text = self.summary(review.Plan(), [resolved_by_bot("marked")])
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

    def test_a_review_with_nothing_to_report_still_posts_a_summary(self) -> None:
        text = self.summary(review.Plan())
        self.assertIn("## ✅ No open findings", text)
        self.assertIn("<!-- pr-review:reviewed=abc123 -->", text)

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


class WorkflowTests(unittest.TestCase):
    """Properties of claude-review.yml that decide who can start a paid review and when. Text checks, since the
    standard library has no YAML parser; actionlint covers the syntax."""

    def setUp(self) -> None:
        if WORKFLOW is None:
            self.skipTest("the workflow is not next to this skill")
        self.text = WORKFLOW.read_text()
        self.authorize_job = "\n  authorize:\n" + self.text.split("\n  authorize:\n")[1].split("\n  review:\n")[0]
        self.review_job = "\n  review:\n" + self.text.split("\n  review:\n")[1].split("\n  status:\n")[0]
        self.status_job = "\n  status:\n" + self.text.split("\n  status:\n")[1]

    def job_if(self, job: str) -> str:
        return job.split("    if: >-\n", 1)[1].split("    runs-on:", 1)[0]

    def test_a_push_does_not_start_a_review(self) -> None:
        condition = self.job_if(self.authorize_job)
        self.assertIn("github.event.action != 'synchronize'", condition)
        triggers = self.text.split("on:\n", 1)[1].split("\n\n", 1)[0]
        self.assertNotIn("reopened", triggers)  # a reopened pull request does not start a review
        self.assertIn("types: [opened, ready_for_review, synchronize]", triggers)

    def test_only_a_pull_request_that_is_ready_and_from_this_repository_is_reviewed_on_open(self) -> None:
        condition = self.job_if(self.authorize_job)
        self.assertIn("github.event.pull_request.draft == false", condition)
        self.assertIn("github.event.pull_request.head.repo.full_name == github.repository", condition)

    def test_a_review_comment_needs_an_organization_member(self) -> None:
        condition = self.job_if(self.authorize_job)
        self.assertIn('contains(fromJSON(\'["OWNER","MEMBER"]\'), github.event.comment.author_association)', condition)
        for association in ("COLLABORATOR", "CONTRIBUTOR", "FIRST_TIMER", "FIRST_TIME_CONTRIBUTOR", "NONE"):
            self.assertNotIn(association, condition)

    def test_the_author_of_a_pull_request_gets_no_special_right_to_ask(self) -> None:
        # Someone outside the organization who opens a pull request from a branch must not be able to spend
        # the review budget by commenting on it.
        self.assertNotIn("github.event.issue.user.login", self.text)
        self.assertNotIn("github.event.comment.user.login ==", self.text)

    def test_write_access_is_checked_in_a_gate_with_no_concurrency_group(self) -> None:
        # GitHub keeps one waiting run per group. A request that is going to be refused must be refused
        # before it joins the review job's group, or it could take the place of an authorized one that waits.
        self.assertNotIn("concurrency:", self.authorize_job)
        self.assertIn("needs: authorize", self.review_job)
        step = self.authorize_job.split("- name: Check the request\n", 1)[1]
        script = step.split("run: |", 1)[1]
        self.assertIn('gh api "repos/$REPO/collaborators/$COMMENTER/permission" --jq \'.user.permissions.push\'', script)
        self.assertIn('if [ "$can_push" != true ]; then', script)
        # An error from the API must count as "no": the fallback is `false`, not an empty string or success.
        self.assertIn("|| echo false)", script)
        # The commenter's name comes through the environment, never into the script text.
        self.assertIn("COMMENTER: ${{ github.event.comment.user.login }}", step)
        self.assertNotIn("${{ github.event.comment", script)
        self.assertIn("allowed: ${{ steps.check.outputs.allowed }}", self.authorize_job)

    def test_a_pull_request_that_already_has_a_review_is_refused_before_the_concurrency_group(self) -> None:
        # Otherwise a draft-to-ready toggle could take the one waiting place and displace a waiting `/review`.
        script = self.authorize_job.split("- name: Check the request\n", 1)[1].split("run: |", 1)[1]
        self.assertIn('startswith("<!-- CLAUDE_REVIEW_SUMMARY -->")', script)
        self.assertIn('"github-actions[bot]"', script)
        self.assertIn("this pull request already has a review", script)
        self.assertIn('echo "allowed=true" >> "$GITHUB_OUTPUT"', script)
        # The refusal must be guarded by the count of existing summaries, and write "allowed=false".
        refusal = script.split('if [ "$existing" -gt 0 ]; then', 1)
        self.assertEqual(len(refusal), 2, "the check that a summary exists must be a real condition")
        self.assertIn('echo "allowed=false" >> "$GITHUB_OUTPUT"', refusal[1].split("fi", 1)[0])
        self.assertIn("exit 0", refusal[1].split("fi", 1)[0])
        # A `/review` comment is not refused for having a review already: that is what it is for.
        comment_branch = script.split("if [ \"$EVENT\" = issue_comment ]; then", 1)[1].split("\n          else\n", 1)[0]
        self.assertNotIn("already has a review", comment_branch)

    def test_the_gate_needs_no_secret_but_the_token_and_no_self_hosted_runner(self) -> None:
        self.assertNotIn("LLM_GATEWAY", self.authorize_job)
        self.assertNotIn("BaseRunnerGroup", self.authorize_job)
        self.assertIn("runs-on: ubuntu-latest", self.authorize_job)

    def test_the_summary_tells_people_who_may_ask_for_a_review(self) -> None:
        self.assertIn("Members of the Base organization with write access", render.RERUN_HELP)
        self.assertNotIn("author", render.RERUN_HELP)

    def test_the_review_job_only_runs_for_a_comment_the_gate_allowed(self) -> None:
        condition = self.job_if(self.review_job)
        self.assertIn("needs.authorize.result == 'success'", condition)
        self.assertIn("needs.authorize.outputs.allowed == 'true'", condition)
        # A skipped `authorize` (a push, a draft, a fork) must skip this job cleanly.
        self.assertIn("!cancelled()", condition)
        # Everything about who and what is decided in the gate, so there is one place to get it right.
        for needle in ("draft", "head.repo", "author_association", "synchronize"):
            self.assertNotIn(needle, condition)

    def test_only_an_exact_review_command_gets_as_far_as_the_concurrency_group(self) -> None:
        condition = self.job_if(self.authorize_job)
        for needle in ("github.event.comment.body == '/review'", "startsWith(github.event.comment.body, '/review ')",
                       "github.event.issue.pull_request", "github.event.issue.state == 'open'",
                       "github.event.comment.user.type != 'Bot'", "fromJSON('\"\\n\"')"):
            self.assertIn(needle, condition)
        self.assertNotIn("startsWith(github.event.comment.body, '/review')", condition)

    def test_the_comment_text_never_reaches_a_shell_script_directly(self) -> None:
        # Interpolating it into `run:` would let a comment inject commands into a job that holds secrets.
        self.assertIn("COMMENT_BODY: ${{ github.event.comment.body }}", self.review_job)
        self.assertNotIn("github.event.comment.body", self.authorize_job.split("steps:", 1)[1])
        for block in self.text.split("run: |")[1:]:
            script = block.split("\n      - ", 1)[0]
            self.assertNotIn("${{ github.event.comment", script)
            self.assertNotIn("${{ github.event.issue.title", script)
            self.assertNotIn("${{ github.event.pull_request.title", script)

    def test_the_pull_request_is_read_from_the_api_and_a_draft_or_fork_is_refused_there_too(self) -> None:
        self.assertIn('gh api "repos/$REPO/pulls/$NUMBER"', self.review_job)
        self.assertIn('[ "$draft" = true ]', self.review_job)
        self.assertIn('"$head_repo" != "$REPO"', self.review_job)
        self.assertIn("ref: ${{ steps.pr.outputs.head_sha }}", self.review_job)

    def test_marking_a_draft_ready_again_does_not_buy_a_second_review(self) -> None:
        self.assertIn("this pull request already has a review", self.review_job)
        self.assertIn('"$EVENT" = pull_request', self.review_job)

    def test_a_second_request_waits_instead_of_cancelling_a_review_in_progress(self) -> None:
        self.assertIn("cancel-in-progress: false", self.review_job)
        self.assertNotIn("cancel-in-progress: true", self.review_job)
        # A comment that is not a review request must not share a group with the review.
        self.assertNotIn("\nconcurrency:", self.text.split("jobs:", 1)[0])

    def test_the_status_job_runs_no_model_and_needs_no_gateway_secret(self) -> None:
        self.assertIn("--refresh-status", self.status_job)
        self.assertNotIn("LLM_GATEWAY", self.status_job)
        self.assertNotIn("ANTHROPIC", self.status_job)
        self.assertNotIn("npm install", self.status_job)
        self.assertIn("github.event.action == 'synchronize'", self.job_if(self.status_job))

    def test_a_review_that_loses_a_race_with_a_push_follows_the_pull_request(self) -> None:
        self.assertIn('[ "$code" -eq 3 ]', self.review_job)
        self.assertIn(f"EXIT_HEAD_MOVED = {review.EXIT_HEAD_MOVED}", (review.SKILL_DIR / "review.py").read_text())

    def step_order(self, job: str) -> list[str]:
        return [line.strip()[len("- name: "):] if "- name:" in line else line.strip()[len("- uses: "):].split("@")[0]
                for line in job.splitlines() if line.startswith("      - ")]

    def test_nothing_from_the_pull_request_is_on_disk_when_the_cli_is_installed(self) -> None:
        # npm reads .npmrc and package.json from the working directory, so installing from inside a checkout of
        # the pull request would let it influence what is installed in a job that holds secrets.
        steps = self.step_order(self.review_job)
        self.assertLess(steps.index("Install Claude Code"), steps.index("actions/checkout"))
        install = self.review_job.split("- name: Install Claude Code\n", 1)[1].split("\n      - ", 1)[0]
        self.assertIn("working-directory: ${{ runner.temp }}", install)

    def test_the_review_script_comes_from_the_default_branch_not_the_pull_requests_base(self) -> None:
        self.assertIn("DEFAULT_BRANCH: ${{ github.event.repository.default_branch }}", self.review_job)
        self.assertIn('git fetch --depth 1 origin "$DEFAULT_BRANCH"', self.review_job)
        self.assertNotIn("base_ref", self.review_job)
        self.assertNotIn("pull_request.base.ref", self.text)

    def test_a_comment_never_runs_the_pull_requests_copy_of_the_script(self) -> None:
        pipeline = self.review_job.split("- name: Check out the review pipeline from the default branch\n", 1)[1]
        pipeline = pipeline.split("\n      - ", 1)[0]
        self.assertIn('elif [ "$EVENT" != pull_request ]; then', pipeline)
        self.assertIn("exit 1", pipeline.split('elif [ "$EVENT" != pull_request ]', 1)[1].split("fi", 1)[0])

    def test_the_status_job_never_checks_out_the_pull_request(self) -> None:
        self.assertIn("ref: ${{ github.event.repository.default_branch }}", self.status_job)
        self.assertNotIn("pull_request.head.sha", self.status_job.split("    steps:", 1)[1])
        self.assertNotIn("pull_request.head.ref", self.status_job)
        self.assertNotIn("git worktree", self.status_job)

    def test_the_status_job_says_so_when_there_is_no_pipeline_on_the_default_branch_yet(self) -> None:
        self.assertIn("if [ ! -f .agents/skills/pr-review/review.py ]; then", self.status_job)

    def test_the_status_command_reads_no_files_from_the_pull_request(self) -> None:
        refresh = (review.SKILL_DIR / "review.py").read_text().split("def refresh_status", 1)[1].split("\ndef ", 1)[0]
        for forbidden in ("git(", "Path(", "open(", "read_text", "cwd"):
            self.assertNotIn(forbidden, refresh)

    def test_the_review_job_is_still_bounded(self) -> None:
        self.assertIn(f"timeout-minutes: {JOB_LIMIT_SECONDS // 60}\n", self.review_job)
        self.assertIn("timeout-minutes: 2\n", self.authorize_job)


class RepeatTests(unittest.TestCase):
    """Findings an earlier round posted, and follow-ups, are handled before the caps and only once posted."""

    def plan(self, actions, **kwargs) -> review.Plan:
        return review.build_plan({"actions": actions}, [thread("open")], review.diff_new_lines(DIFF), **kwargs)

    def test_a_repeat_takes_no_inline_slot_so_a_new_finding_gets_it(self) -> None:
        seen = {("src/a.rs", f"old {i}") for i in range(3)}
        actions = [comment(title=f"old {i}", line=1 + i % 3) for i in range(3)] + [comment(title="new", line=2)]
        plan = self.plan(actions, inline_room=1, seen=seen)
        self.assertEqual([f.title for f in plan.new], ["new"])
        self.assertEqual(plan.outside, [])

    def test_a_repeat_within_one_round_is_dropped(self) -> None:
        plan = self.plan([comment(title="Same", line=1), comment(title="same", line=2)])
        self.assertEqual(len(plan.new), 1)

    def test_the_same_title_in_another_file_is_a_different_finding(self) -> None:
        plan = self.plan([comment(title="Same"), comment(title="Same", path="src/b.rs", line=1)],
                         seen=set())
        self.assertEqual(len(plan.new) + len(plan.outside), 2)

    def test_build_plan_does_not_record_anything_itself(self) -> None:
        seen: set = set()
        replied: set = set()
        self.plan([comment(title="x"), {"type": "reply", "thread_id": "open", "body": "again"}],
                  seen=seen, replied=replied)
        self.assertEqual((seen, replied), (set(), set()))

    def test_a_follow_up_that_was_not_posted_does_not_block_a_later_round(self) -> None:
        # A round's reply is rejected as a repeat only if an earlier round actually posted it.
        first = self.plan([{"type": "reply", "thread_id": "open", "body": "a"}], replied=set())
        self.assertEqual(len(first.replies), 1)
        retry = self.plan([{"type": "reply", "thread_id": "open", "body": "b"}], replied=set())
        self.assertEqual(len(retry.replies), 1)
        posted = self.plan([{"type": "reply", "thread_id": "open", "body": "b"}], replied={"open"})
        self.assertEqual((posted.replies, len(posted.rejected)), ([], 1))

    def test_apply_plan_keeps_only_the_replies_it_posted(self) -> None:
        ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha="abc")
        plan = self.plan([{"type": "reply", "thread_id": "open", "body": "a"}])
        gh = FakeGh(fail=("addPullRequestReviewThreadReply",))
        with mock.patch.object(review, "gh", gh), mock.patch.object(review, "fetch_threads",
                                                                      return_value=[thread("open")]):
            review.apply_plan(plan, ctx)
        self.assertEqual(plan.replies, [])


class StatusTests(unittest.TestCase):
    """The reviewed commit, the count of commits after it, and the line that says so."""

    REVIEWED = "a" * 40

    def summary_body(self, **extra) -> str:
        return render.render_summary(overview=None, new=[], outside=[], threads=[], reopened=set(), fixed=set(),
                                     failed=[], details="d", repo="base/base", head_sha=self.REVIEWED, **extra)

    def test_a_summary_records_the_commit_it_reviewed(self) -> None:
        self.assertEqual(render.reviewed_sha(self.summary_body()), self.REVIEWED)
        self.assertIsNone(render.reviewed_sha("<!-- CLAUDE_REVIEW_SUMMARY -->\n## old summary"))

    def test_a_summary_says_how_to_ask_for_another_review(self) -> None:
        text = self.summary_body()
        self.assertIn("comment `/review`", text)
        self.assertTrue(text.rstrip().endswith("can do this."))

    def test_a_local_report_has_no_rerun_instructions_or_reviewed_marker(self) -> None:
        text = render.render_summary(overview=None, new=[], outside=[], threads=[], reopened=set(), fixed=set(),
                                     failed=[], details="d", repo="base/base", head_sha=None)
        self.assertNotIn("/review", text)
        self.assertNotIn("pr-review:reviewed", text)

    def test_the_rerun_instructions_survive_a_very_long_summary(self) -> None:
        findings = [render.Finding.from_action(comment(line=99, body="z" * 5000, title=f"t{i}")) for i in range(40)]
        text = render.render_summary(overview=None, new=[], outside=findings, threads=[], reopened=set(),
                                     fixed=set(), failed=[], details="d", repo="base/base", head_sha=self.REVIEWED)
        self.assertLessEqual(len(text), render.MAX_BODY_CHARS + 1)
        self.assertIn("comment `/review`", text)
        self.assertEqual(render.reviewed_sha(text), self.REVIEWED)

    def test_the_status_line_for_a_long_pull_request(self) -> None:
        block = render.status_block(self.REVIEWED, unreviewed=None, compare_url="c", files_url="https://x/files",
                                    uncountable=True)
        self.assertIn("too many commits to count", block)
        self.assertIn("[View all changes](https://x/files)", block)
        self.assertNotIn("rewritten", block)

    def test_the_status_line_for_each_case(self) -> None:
        def block(unreviewed):
            return render.status_block(self.REVIEWED, unreviewed=unreviewed, compare_url="https://x/compare",
                                       files_url="https://x/files")

        self.assertIn("the latest commit", block(0))
        one = block(1)
        self.assertIn("**1 commit pushed after `aaaaaaa` has not been reviewed.**", one)
        self.assertIn("[View the diff](https://x/compare)", one)
        self.assertIn("to review it", one)
        many = block(3)
        self.assertIn("**3 commits pushed after `aaaaaaa` have not been reviewed.**", many)
        self.assertIn("to review them", many)
        rewritten = block(None)
        self.assertIn("rewritten", rewritten)
        self.assertIn("[View all changes](https://x/files)", rewritten)

    def test_replacing_the_status_block_changes_only_that_block(self) -> None:
        body = self.summary_body()
        new = render.status_block(self.REVIEWED, unreviewed=2, compare_url="https://x/c", files_url="https://x/f")
        replaced = render.replace_status(body, new)
        self.assertIn("2 commits pushed", replaced)
        self.assertEqual(render.reviewed_sha(replaced), self.REVIEWED)
        self.assertEqual(replaced.count(render.STATUS_START), 1)
        before, after = body.split(render.STATUS_START)[0], body.split(render.STATUS_END)[1]
        self.assertTrue(replaced.startswith(before) and replaced.endswith(after))
        self.assertIsNone(render.replace_status("an older summary with no status block", new))

    def test_the_summary_embeds_a_given_status_line(self) -> None:
        block = render.status_block(self.REVIEWED, unreviewed=4, compare_url="https://x/c", files_url="https://x/f")
        self.assertIn("4 commits pushed", self.summary_body(status=block))


class CommitCountTests(unittest.TestCase):
    SHAS = ["1" * 40, "2" * 40, "3" * 40, "4" * 40]

    def fake_gh(self, shas, head=None, body=None):
        calls: list[list[str]] = []

        def gh(args, input_text=None):
            calls.append(args)
            joined = " ".join(args)
            if "pulls/7/commits" in joined:
                half = len(shas) // 2
                return json.dumps([[{"sha": s} for s in shas[:half]], [{"sha": s} for s in shas[half:]]])
            if args[:2] == ["pr", "view"]:
                return json.dumps({"headRefOid": head or shas[-1]})
            if "startswith" in joined:
                return "" if body is None else json.dumps({"id": 55, "body": body}) + "\n"
            return ""

        return gh, calls

    def test_commits_after_counts_across_pages(self) -> None:
        gh, _ = self.fake_gh(self.SHAS)
        with mock.patch.object(review, "gh", gh):
            self.assertEqual([review.commits_after(7, "base/base", s) for s in self.SHAS], [3, 2, 1, 0])
            self.assertEqual(review.commits_after(7, "base/base", self.SHAS[1][:7]), 2)

    def test_a_pull_request_with_more_commits_than_github_lists_cannot_be_counted(self) -> None:
        many = [f"{i:040x}" for i in range(review.COMMIT_LIST_LIMIT)]
        gh, _ = self.fake_gh(many)
        with mock.patch.object(review, "gh", gh):
            # The reviewed commit may be in the list, but the newest commits are cut off, so any count is wrong.
            for reviewed in (many[0], many[-1], "f" * 40):
                with self.subTest(reviewed=reviewed[:7]), self.assertRaises(review.CommitsUncountable):
                    review.commits_after(7, "base/base", reviewed)

    def test_the_summary_says_so_instead_of_claiming_the_branch_was_rewritten(self) -> None:
        many = [f"{i:040x}" for i in range(review.COMMIT_LIST_LIMIT)]
        gh, _ = self.fake_gh(many, head="e" * 40)
        ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha=many[3])
        with mock.patch.object(review, "gh", gh):
            block = review.status_for(ctx)
        self.assertIn("too many commits to count", block)
        self.assertNotIn("rewritten", block)
        self.assertIn("/pull/7/files", block)

    def test_refresh_status_says_so_for_a_long_pull_request(self) -> None:
        many = [f"{i:040x}" for i in range(review.COMMIT_LIST_LIMIT)]
        gh, _ = self.fake_gh(many, head="e" * 40, body=self.body(many[3]))
        sent: list[str] = []

        def capture(args, input_text=None):
            if "PATCH" in args:
                sent.append(input_text)
                return ""
            return gh(args, input_text)

        with mock.patch.object(review, "gh", capture):
            review.refresh_status(7, "base/base")
        self.assertIn("too many commits to count", json.loads(sent[0])["body"])

    def test_a_commit_no_longer_in_the_pull_request_means_the_branch_was_rewritten(self) -> None:
        gh, _ = self.fake_gh(self.SHAS)
        with mock.patch.object(review, "gh", gh):
            self.assertIsNone(review.commits_after(7, "base/base", "9" * 40))

    def body(self, sha: str, *, current: bool = True) -> str:
        """A summary of `sha`; by default one whose status line already says it is the latest commit."""
        status = render.status_block(sha, unreviewed=0, compare_url="", files_url="") if current else None
        return render.render_summary(overview=None, new=[], outside=[], threads=[], reopened=set(), fixed=set(),
                                     failed=[], details="d", repo="base/base", head_sha=sha, status=status)

    def test_refresh_status_edits_the_latest_summary_with_the_count_and_a_compare_link(self) -> None:
        gh, calls = self.fake_gh(self.SHAS, body=self.body(self.SHAS[1]))
        sent: list[str] = []
        real = gh

        def capture(args, input_text=None):
            if "PATCH" in args:
                sent.append(input_text)
                return ""
            return real(args, input_text)

        with mock.patch.object(review, "gh", capture):
            self.assertEqual(review.refresh_status(7, "base/base"), 0)
        [payload] = sent
        body = json.loads(payload)["body"]
        self.assertIn("2 commits pushed after `2222222` have not been reviewed", body)
        self.assertIn(f"https://github.com/base/base/compare/{self.SHAS[1]}...{self.SHAS[3]}", body)

    def test_refresh_status_does_nothing_when_up_to_date_or_without_a_summary(self) -> None:
        for body in (None, self.body(self.SHAS[3])):
            with self.subTest(has_summary=body is not None):
                gh, calls = self.fake_gh(self.SHAS, body=body)
                with mock.patch.object(review, "gh", gh):
                    self.assertEqual(review.refresh_status(7, "base/base"), 0)
                self.assertFalse([c for c in calls if "PATCH" in c])

    def test_a_summary_that_could_not_check_for_newer_commits_is_corrected_by_the_next_refresh(self) -> None:
        sent: list[str] = []
        gh, _ = self.fake_gh(self.SHAS, body=self.body(self.SHAS[3], current=False))

        def capture(args, input_text=None):
            if "PATCH" in args:
                sent.append(json.loads(input_text)["body"])
                return ""
            return gh(args, input_text)

        with mock.patch.object(review, "gh", capture):
            review.refresh_status(7, "base/base")
        self.assertEqual(len(sent), 1)
        self.assertIn("the latest commit", sent[0])
        self.assertNotIn("could not be checked", sent[0])

    def test_refresh_status_leaves_a_summary_from_before_the_marker_existed_alone(self) -> None:
        gh, calls = self.fake_gh(self.SHAS, body="<!-- CLAUDE_REVIEW_SUMMARY -->\n## old")
        with mock.patch.object(review, "gh", gh):
            self.assertEqual(review.refresh_status(7, "base/base"), 0)
        self.assertFalse([c for c in calls if "PATCH" in c])

    def test_refresh_status_uses_the_latest_of_several_summaries(self) -> None:
        old, new = self.body(self.SHAS[0]), self.body(self.SHAS[2])
        sent: list[str] = []

        def gh(args, input_text=None):
            joined = " ".join(args)
            if "startswith" in joined:
                return json.dumps({"id": 10, "body": old}) + "\n" + json.dumps({"id": 20, "body": new}) + "\n"
            if "pulls/7/commits" in joined:
                return json.dumps([[{"sha": s} for s in self.SHAS]])
            if args[:2] == ["pr", "view"]:
                return json.dumps({"headRefOid": self.SHAS[3]})
            if "PATCH" in args:
                sent.append(joined)
            return ""

        with mock.patch.object(review, "gh", gh):
            review.refresh_status(7, "base/base")
        self.assertEqual(len(sent), 1)
        self.assertIn("issues/comments/20", sent[0])

    def test_the_summary_and_the_refresh_agree_for_a_short_and_a_full_sha(self) -> None:
        # They used to differ: one matched the reviewed commit by prefix, the other only exactly.
        gh, _ = self.fake_gh(self.SHAS, head=self.SHAS[3])
        sent: list[str] = []

        def capture(args, input_text=None):
            if "PATCH" in args:
                sent.append(json.loads(input_text)["body"])
                return ""
            return gh(args, input_text)

        for reviewed in (self.SHAS[1], self.SHAS[1][:7]):
            with self.subTest(reviewed=len(reviewed)):
                ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha=reviewed)
                with mock.patch.object(review, "gh", capture):
                    from_summary = review.status_for(ctx)
                    from_refresh = review.status_line(7, "base/base", reviewed)
                self.assertEqual(from_summary, from_refresh)
                self.assertIn("2 commits pushed after `2222222`", from_summary)

    def test_a_short_sha_equal_to_the_head_counts_as_the_latest_commit_in_both_places(self) -> None:
        gh, _ = self.fake_gh(self.SHAS, head=self.SHAS[1])
        ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha=self.SHAS[1][:7])
        with mock.patch.object(review, "gh", gh):
            self.assertIn("the latest commit", review.status_for(ctx))
            self.assertIn("the latest commit", review.status_line(7, "base/base", self.SHAS[1][:7]))

    def test_status_for_counts_against_the_live_head(self) -> None:
        gh, _ = self.fake_gh(self.SHAS, head=self.SHAS[3])
        ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha=self.SHAS[1])
        with mock.patch.object(review, "gh", gh):
            block = review.status_for(ctx)
        self.assertIn("2 commits pushed after `2222222`", block)

    def test_status_for_says_latest_when_the_head_has_not_moved_and_nothing_when_it_cannot_ask(self) -> None:
        gh, _ = self.fake_gh(self.SHAS, head=self.SHAS[1])
        ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha=self.SHAS[1])
        with mock.patch.object(review, "gh", gh):
            self.assertIn("the latest commit", review.status_for(ctx))
        self.assertIsNone(review.status_for(review.Context(description="d", title="t", files=[], diff=DIFF)))

    def test_when_the_pull_request_cannot_be_read_the_summary_does_not_claim_to_be_current(self) -> None:
        ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha=self.SHAS[1])
        for failure in ("pr view", "pulls/7/commits"):
            with self.subTest(failure=failure), mock.patch.object(review, "gh", FakeGh(fail=(failure,))):
                block = review.status_for(ctx)
            self.assertIn("could not be checked", block)
            self.assertNotIn("✅", block)  # "✅ Reviewed ..., the latest commit." is the claim of being current
            self.assertIn("comment `/review`", block)

    def test_a_summary_built_without_any_status_does_not_claim_to_be_current_either(self) -> None:
        text = render.render_summary(overview=None, new=[], outside=[], threads=[], reopened=set(), fixed=set(),
                                     failed=[], details="d", repo="base/base", head_sha="b" * 40)
        self.assertIn("could not be checked", text)
        self.assertNotIn("✅ Reviewed", text)

    def test_a_local_run_makes_no_status_calls(self) -> None:
        self.assertIsNone(review.status_for(review.Context(description="d", title="t", files=[], diff=DIFF)))

    def test_the_cli_requires_a_pull_request_for_refresh_status_and_rejects_mixing_it_with_post(self) -> None:
        for argv in (["--refresh-status"], ["--pr", "7", "--post", "--refresh-status"]):
            with self.subTest(argv=argv), self.assertRaises(SystemExit):
                review.parse_args(argv)
        self.assertTrue(review.parse_args(["--pr", "7", "--refresh-status"]).refresh_status)


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

    def test_posting_from_the_wrong_commit_is_refused_with_its_own_error(self) -> None:
        gh_fn, _ = self.scripted()
        with self.assertRaises(review.HeadMovedError):
            self.context(gh_fn, checked_out="b" * 40, post=True)

    def test_that_error_makes_the_script_exit_with_a_status_the_workflow_can_act_on(self) -> None:
        with mock.patch.object(review, "load_agents", return_value=[]), mock.patch.object(
                review, "pr_context", side_effect=review.HeadMovedError("moved")):
            self.assertEqual(review.main(["--pr", "7", "--post"]), review.EXIT_HEAD_MOVED)
        with mock.patch.object(review, "load_agents", return_value=[]), mock.patch.object(
                review, "pr_context", side_effect=review.ReviewError("other")):
            self.assertEqual(review.main(["--pr", "7", "--post"]), 1)

    def test_a_local_run_on_another_commit_only_warns(self) -> None:
        gh_fn, _ = self.scripted()
        self.assertEqual(self.context(gh_fn, checked_out="b" * 40, post=False).head_sha, self.HEAD)


class LimitTests(unittest.TestCase):
    def test_agents_have_no_time_limit_unless_their_file_sets_one(self) -> None:
        for agent in review.load_agents():
            with self.subTest(agent=agent.name):
                self.assertIsNone(agent.timeout_seconds)

    def test_a_limit_in_an_agent_file_is_still_honoured(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.md"
            path.write_text("---\nstage: review\nmodel: m\ntimeout_seconds: 90\n---\nx")
            self.assertEqual(review.parse_agent(path).timeout_seconds, 90)

    def test_the_cli_is_run_without_a_timeout_when_the_agent_has_none(self) -> None:
        agent = review.Agent("a", "review", ("always",), "m", "high", "Read", None, None, None, "p", Path("a.md"))
        envelope = json.dumps({"is_error": False, "structured_output": {"findings": []}, "total_cost_usd": 0,
                               "modelUsage": {"m": {}}})
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(review, "run", return_value=envelope) as run:
            review.run_agent(agent, "p", Path(tmp), Path(tmp), None)
        self.assertIsNone(run.call_args.kwargs["timeout"])

    def test_the_review_has_no_whole_run_budget(self) -> None:
        self.assertFalse(hasattr(review, "Budget"))
        self.assertFalse(hasattr(review, "DEFAULT_BUDGET_SECONDS"))

    def test_the_job_limit_in_the_workflow_is_the_backstop(self) -> None:
        if WORKFLOW is not None:
            self.assertIn(f"timeout-minutes: {JOB_LIMIT_SECONDS // 60}\n", WORKFLOW.read_text())

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


def find_workflow() -> Path | None:
    """The review workflow, found by walking up from this file; None if the skill was copied elsewhere."""
    for parent in Path(__file__).resolve().parents:
        candidate = parent / ".github" / "workflows" / "claude-review.yml"
        if candidate.exists():
            return candidate
    return None


WORKFLOW = find_workflow()
JOB_LIMIT_SECONDS = 30 * 60  # timeout-minutes in claude-review.yml; a test checks that the two agree


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

    def test_agents_that_explore_are_told_to_work_fast_and_the_ones_that_judge_are_not(self) -> None:
        for stage in review.STAGES:
            with self.subTest(stage=stage):
                _, run = self.call(self.agent(stage=stage), [self.envelope()])
                cmd = run.call_args.args[0]
                prompt = cmd[cmd.index("--append-system-prompt") + 1]
                self.assertEqual("## Working fast" in prompt, stage in review.EXPLORING_STAGES)

    def test_a_ballot_is_not_capped_even_though_a_council_member_casts_it(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(review, "run", return_value=self.envelope()) as run:
            review.run_agent(self.agent(stage="council"), "p", Path(tmp), Path(tmp), None,
                             schema="votes", label="m.vote")
        cmd = run.call_args.args[0]
        self.assertNotIn("## Working fast", cmd[cmd.index("--append-system-prompt") + 1])

    def test_a_cut_off_diff_names_the_files_it_lost(self) -> None:
        def file_diff(name: str) -> str:
            return f"diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n@@ -1 +1 @@\n-x\n+y\n"

        full = "".join(file_diff(n) for n in ("a.rs", "b.rs", "c.rs", "d.rs"))
        cut = full[: full.index("diff --git a/c.rs") + 20]  # the cut lands inside c.rs
        self.assertEqual(review.omitted_files(full, cut), ["c.rs", "d.rs"])
        self.assertEqual(review.omitted_files(full, full), ["d.rs"])  # the last file of a whole diff is rechecked
        self.assertEqual(review.omitted_files(full, ""), ["a.rs", "b.rs", "c.rs", "d.rs"])

    def test_the_change_block_lists_the_cut_off_files(self) -> None:
        names = [f"f{i}.rs" for i in range(5)]
        diff = "".join(f"diff --git a/{n} b/{n}\n--- a/{n}\n+++ b/{n}\n@@ -1 +1 @@\n-x\n+y\n" for n in names)
        ctx = review.Context(description="d", title="t", files=names, diff=diff)
        with mock.patch.object(review, "MAX_DIFF_CHARS", len(diff) // 2):
            text = review.change_block(ctx)
        self.assertIn("The diff was cut off here", text)
        self.assertIn("f4.rs", text)
        self.assertNotIn("f0.rs\n", text.split("cut off here")[1])
        with mock.patch.object(review, "MAX_DIFF_CHARS", len(diff) + 1):
            self.assertNotIn("cut off", review.change_block(ctx))

    def test_the_decider_and_chair_are_never_capped_on_tool_calls(self) -> None:
        # They must check every open thread and every claim, however many reads that takes.
        self.assertFalse({"chair", "decide"} & review.EXPLORING_STAGES)

    def test_the_working_guide_has_no_stale_time_claims_and_covers_truncated_diffs(self) -> None:
        text = review.WORKING_GUIDE.read_text()
        self.assertNotIn("hard time limit", text)
        # The guide must use the same words as the note that change_block adds to a cut-off diff.
        self.assertIn("cut off", text)

    def test_pr_settings_are_never_loaded(self) -> None:
        _, run = self.call(self.agent(), [self.envelope()])
        cmd = run.call_args.args[0]
        self.assertEqual(cmd[cmd.index("--setting-sources") + 1], "user")

    def test_the_model_that_really_ran_is_recorded(self) -> None:
        review.MODELS_RAN.clear()
        self.call(self.agent(name="alias-agent", model="opus"), [self.envelope()])
        self.assertEqual(review.MODELS_RAN["alias-agent"], "resolved-model")

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
        self.live = list(self.threads)  # what GitHub has when apply_plan looks again

    def apply(self, gh: FakeGh) -> list[str]:
        def summarize(plan: review.Plan) -> str | None:
            return render.render_summary(
                overview=None, new=plan.new, outside=plan.outside, threads=self.ctx.threads,
                reopened={u["thread_id"] for u in plan.reopens}, fixed={r["thread_id"] for r in plan.resolves},
                back=set(plan.back), failed=[], details="d", repo="base/base", head_sha="abc",
                )

        gh.responses.setdefault("pr view", json.dumps({"headRefOid": "abc"}))
        def live_threads(*_):
            if self.live is None:
                raise review.ReviewError("graphql: boom")
            return self.live

        with mock.patch.object(review, "gh", gh), mock.patch.object(
                review, "fetch_threads", side_effect=live_threads):
            return review.apply_plan(self.plan, self.ctx, summarize)

    def test_everything_is_posted_and_the_old_summary_is_replaced_after_the_new_one(self) -> None:
        summaries = "".join(json.dumps({"id": i, "body": "<!-- CLAUDE_REVIEW_SUMMARY -->\nold"}) + "\n" for i in (11, 12))
        gh = FakeGh(responses={"issues/7/comments": summaries})
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

    def summary_of(self, gh: FakeGh) -> str:
        [summary] = [i for c, i in zip(gh.calls, gh.inputs, strict=True) if c[:2] == ["pr", "comment"]]
        return summary

    def test_a_reopen_whose_edit_fails_keeps_the_thread_open_in_the_summary(self) -> None:
        gh = FakeGh(fail=("comments/100",))
        self.apply(gh)
        self.assertEqual((self.plan.reopens, self.plan.back), ([], ["marked"]))
        text = self.summary_of(gh)
        self.assertIn("Problem is back, but the comment could not be restored", text)
        self.assertNotIn("No open findings", text)

    def test_when_the_live_read_fails_nothing_is_changed_but_everything_else_is_posted(self) -> None:
        self.live = None
        gh = FakeGh()
        problems = self.apply(gh)
        self.assertEqual(len(problems), 1)
        self.assertIn("re-read threads", problems[0])
        self.assertEqual([c for c in gh.calls if "PATCH" in c], [])
        self.assertEqual(len(gh.matching("pulls/7/reviews")), 1)
        text = self.summary_of(gh)
        self.assertNotIn("### Fixed in this push", text)
        self.assertIn("Problem is back, but the comment could not be restored", text)

    def test_the_summary_is_written_from_the_threads_as_they_are_now(self) -> None:
        # A person resolved this open thread while the agents ran; it must not be counted as open.
        self.live = [{**t, "resolved": True} if t["thread_id"] == "open" else t for t in self.threads]
        text = self.summary_of(self.run_apply())
        self.assertNotIn("A problem.", text.split("How this was reviewed")[0].split("Fixed in this push")[0])

    def run_apply(self) -> FakeGh:
        gh = FakeGh()
        self.apply(gh)
        return gh

    def test_a_reopen_whose_reply_fails_still_counts_because_the_comment_is_restored(self) -> None:
        gh = FakeGh(fail=("addPullRequestReviewThreadReply",))
        problems = self.apply(gh)
        self.assertEqual(len(self.plan.reopens), 1)
        self.assertTrue([p for p in problems if "reply on" in p])
        [summary] = [i for c, i in zip(gh.calls, gh.inputs, strict=True) if c[:2] == ["pr", "comment"]]
        self.assertIn("### Reopened", summary)

    def test_a_thread_a_person_resolved_during_the_run_is_left_alone(self) -> None:
        self.live = [{**t, "resolved": True} if t["thread_id"] == "marked" else t for t in self.threads]
        gh = FakeGh()
        self.apply(gh)
        self.assertEqual(self.plan.reopens, [])
        self.assertEqual([c for c in gh.calls if "PATCH" in c and "comments/100" in " ".join(c)], [])
        self.assertFalse([c for c in gh.matching("addPullRequestReviewThreadReply") if "Reopened" in " ".join(c)])

    def test_a_comment_edited_during_the_run_is_left_alone(self) -> None:
        self.live = [{**t, "first_body": t["first_body"] + "\n\nA person added this."}
                     if t["thread_id"] == "fixed" else t for t in self.threads]
        gh = FakeGh()
        self.apply(gh)
        self.assertEqual(self.plan.resolves, [])
        self.assertEqual([c for c in gh.calls if "PATCH" in c and "comments/22" in " ".join(c)], [])

    def test_a_reopen_dropped_because_the_comment_changed_stays_in_the_summary(self) -> None:
        self.live = [{**t, "first_body": t["first_body"] + "\n\nEdited."} if t["thread_id"] == "marked" else t
                     for t in self.threads]
        gh = FakeGh()
        self.apply(gh)
        self.assertEqual((self.plan.reopens, self.plan.back), ([], ["marked"]))
        self.assertIn("Problem is back, but the comment could not be restored", self.summary_of(gh))

    def test_a_follow_up_is_not_posted_on_a_thread_a_person_resolved_or_deleted(self) -> None:
        for label, live in (("resolved", lambda t: {**t, "resolved": True}), ("deleted", None)):
            with self.subTest(label):
                self.live = [(live(t) if live else None) if t["thread_id"] == "open" else t for t in self.threads]
                self.live = [t for t in self.live if t is not None]
                gh = FakeGh()
                self.apply(gh)
                follow_ups = [c for c in gh.matching("addPullRequestReviewThreadReply") if "Follow-up" in " ".join(c)]
                self.assertEqual(follow_ups, [])

    def test_a_follow_up_is_posted_when_the_thread_is_still_open(self) -> None:
        gh = FakeGh()
        self.apply(gh)
        self.assertTrue([c for c in gh.matching("addPullRequestReviewThreadReply") if "Follow-up" in " ".join(c)])

    def test_no_follow_up_is_posted_without_a_current_view_of_the_threads(self) -> None:
        self.live = None
        gh = FakeGh()
        self.apply(gh)
        self.assertEqual(gh.matching("addPullRequestReviewThreadReply"), [])

    def test_a_thread_deleted_during_the_run_is_left_alone(self) -> None:
        self.live = [t for t in self.threads if t["thread_id"] != "fixed"]
        self.apply(FakeGh())
        self.assertEqual(self.plan.resolves, [])

    def test_a_thread_the_bot_marked_during_the_run_is_not_marked_again(self) -> None:
        self.live = [resolved_by_bot("fixed") if t["thread_id"] == "fixed" else t for t in self.threads]
        self.apply(FakeGh())
        self.assertEqual(self.plan.resolves, [])

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

    def test_a_push_during_the_run_does_not_stop_the_findings_but_stops_claims_that_something_is_fixed(self) -> None:
        # Findings are about the commit that was read and are anchored to it. "Fixed" is a claim about the
        # head: a later commit may have brought the problem back.
        gh = FakeGh(responses={"pr view": json.dumps({"headRefOid": "newer"})})
        self.apply(gh)
        self.assertEqual(len(gh.matching("pulls/7/reviews")), 1)
        self.assertEqual([c for c in gh.calls if "PATCH" in c and "comments/22" in " ".join(c)], [])
        self.assertEqual(self.plan.resolves, [])
        self.assertIn("pr view", " ".join(" ".join(c) for c in gh.calls))

    def test_a_resolve_is_kept_while_the_head_is_still_the_reviewed_commit(self) -> None:
        gh = FakeGh(responses={"pr view": json.dumps({"headRefOid": "abc"})})
        self.apply(gh)
        self.assertEqual(len(self.plan.resolves), 1)

    def test_when_the_head_cannot_be_read_nothing_is_marked_fixed_and_the_rest_still_posts(self) -> None:
        gh = FakeGh(fail=("pr view",))
        problems = self.apply(gh)
        self.assertEqual(self.plan.resolves, [])
        self.assertTrue([p for p in problems if "check the pull request head" in p])
        self.assertEqual(len(gh.matching("pulls/7/reviews")), 1)

    def test_a_run_with_no_resolves_does_not_need_to_ask_for_the_head(self) -> None:
        self.plan.resolves = []
        gh = FakeGh(fail=("pr view",))
        problems = self.apply(gh)
        self.assertFalse([p for p in problems if "check the pull request head" in p])

    def test_a_malformed_thread_response_does_not_end_the_run(self) -> None:
        for error in (ValueError("bad json"), KeyError("data"), TypeError("not a list")):
            with self.subTest(error=type(error).__name__):
                ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha="abc")
                gh = FakeGh(responses={"pr view": json.dumps({"headRefOid": "abc"})})
                plan = review.Plan(new=[render.Finding.from_action(comment(title="kept"))])
                with mock.patch.object(review, "gh", gh), mock.patch.object(review, "fetch_threads", side_effect=error):
                    problems = review.apply_plan(plan, ctx)
                self.assertTrue([p for p in problems if "re-read threads" in p])
                self.assertEqual(len(gh.matching("pulls/7/reviews")), 1)

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
        # The follow-up on one thread and the reply that goes with reopening another. The reopened
        # thread still counts: its comment was put back, only the explanation failed to post.
        self.assertEqual(len(problems), 2)
        self.assertEqual(len(self.plan.reopens), 1)
        self.assertEqual(len(gh.matching("pulls/7/reviews")), 1)
        self.assertEqual(len([c for c in gh.calls if c[:2] == ["pr", "comment"]]), 1)

    def test_a_failed_summary_post_keeps_the_old_summary_and_is_reported(self) -> None:
        old = json.dumps({"id": 11, "body": "<!-- CLAUDE_REVIEW_SUMMARY -->\nold"}) + "\n"
        gh = FakeGh(fail=("pr comment",), responses={"issues/7/comments": old})
        problems = self.apply(gh)
        self.assertEqual(len(problems), 1)
        self.assertEqual(gh.matching("DELETE"), [])
        # Everything else was still posted, so one failed write does not lose the rest.
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
        self.decisions: dict[str, dict] = {}
        self.triage = {"depth": "deep", "block_production_sensitive": False, "reasoning": "r", "focus_areas": []}
        self.finding = {"title": "t", "severity": "major", "category": "safety", "confidence": "high",
                        "path": "src/a.rs", "line": 1, "explanation": "e"}

    def fake_run_agent(self, agent, prompt, cwd, artifacts, model_override, *, schema=None, label=None,
                       **_: object) -> dict:
        label = label or agent.name
        self.calls.append(label)
        if label in self.failing:
            raise review.ReviewError(f"{label} broke")
        review.MODELS_RAN[agent.name] = model_override or agent.model  # what the real run_agent records
        if agent.stage == "triage":
            return self.triage
        if schema == "votes":
            return {"votes": []}
        if agent.stage == "chair":
            return {"findings": []}
        if agent.stage == "decide":
            return self.decisions.get(label, {"actions": [], "overview": None, "dropped": []})
        return {"findings": [self.finding] if agent.name == "council-adversary" else []}

    def run_pipeline(self, post: bool = False) -> review.Outcome:
        with mock.patch.object(review, "run_agent", self.fake_run_agent):
            return review.run_pipeline(self.ctx, self.agents, Path("."), Path("."), None, post=post)

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

    def test_the_summary_does_not_claim_an_agent_that_failed_ran(self) -> None:
        self.failing = {"council-design"}
        outcome = self.run_pipeline()
        models = {name: model for _, name, model in outcome.rows}
        self.assertEqual(models["council-design"], "(did not finish)")
        self.assertNotEqual(models["council-invariants"], "(did not finish)")
        self.assertNotIn("(did not finish)", models["triage"])

    def test_one_member_failing_does_not_stop_the_council(self) -> None:
        self.failing = {"council-design"}
        outcome = self.run_pipeline()
        self.assertIn("council/council-design", outcome.failed)
        self.assertIn("council-chair", self.calls)

    def test_a_failed_final_round_still_yields_a_summary_that_says_so(self) -> None:
        self.failing = {"decide"}
        outcome = self.run_pipeline()
        self.assertIn("decide", outcome.failed)
        self.assertIn("final step failed", outcome.decision["overview"])

    def test_every_reviewer_failing_is_an_error(self) -> None:
        self.failing = {"review-general", "council-invariants", "council-adversary", "council-design"}
        with self.assertRaises(review.ReviewError):
            self.run_pipeline()


def comment_on_line_one(title: str, **extra) -> dict:
    return {"type": "comment", "path": "src/a.rs", "line": 1, "title": title, "body": f"{title} happens.",
            "severity": "major", "category": "safety", **extra}


class RoundTests(unittest.TestCase):
    """Findings are decided and posted as each reviewer finishes; a final round does the threads."""

    def setUp(self) -> None:
        self.ctx = review.Context(description="d", title="t", files=["src/a.rs"], diff=DIFF, pr_number=7,
                                  head_sha="abc", threads=[thread("open", comment_id=11)])
        self.agents = review.load_agents()
        self.events: list[str] = []
        self.release = threading.Event()
        self.round_decisions: dict[str, dict] = {}
        self.final_decision: dict = {"actions": [], "overview": "Done.", "dropped": []}
        self.slow = "council-invariants"
        self.broken: set[str] = set()
        self.finding = {"title": "f", "severity": "major", "category": "safety", "confidence": "high",
                        "path": "src/a.rs", "line": 1, "explanation": "e"}

    def fake_run_agent(self, agent, prompt, cwd, artifacts, model_override, *, schema=None, label=None, **_):
        label = label or agent.name
        if agent.stage == "decide" and label == "decide.review-general":
            self.release.set()  # the slow reviewer may finish once the first round has been attempted
        if label in self.broken:
            raise review.ReviewError(f"{label} broke")
        review.MODELS_RAN[agent.name] = agent.model
        if agent.stage == "triage":
            return {"depth": "standard", "block_production_sensitive": False, "reasoning": "r", "focus_areas": []}
        if agent.stage == "decide":
            if label == "decide":
                self.events.append("final-round")
                return self.final_decision
            self.events.append(f"round:{label.removeprefix('decide.')}")
            return self.round_decisions.get(label, {"actions": [], "overview": None, "dropped": []})
        if agent.name == self.slow and self.finding is not None:
            assert self.release.wait(10), "the first findings were not decided while this reviewer was running"
        return {"findings": [self.finding] if self.finding is not None else []}

    def apply(self, round_plan: review.Plan, ctx, summarize=None) -> list[str]:
        self.events.append(f"post:{len(round_plan.new)}c/{len(round_plan.replies)}r")
        return []

    def run_pipeline(self, post: bool = True, reviewers: str = "review-general") -> review.Outcome:
        # A standard change with the general reviewer, plus a second reviewer that is slow.
        agents = [a for a in self.agents if a.stage in ("triage", "decide")]
        general = next(a for a in self.agents if a.name == "review-general")
        slow = dataclasses_replace(general, name=self.slow)
        agents += [general, slow]
        with mock.patch.object(review, "run_agent", self.fake_run_agent), mock.patch.object(
                review, "apply_plan", self.apply):
            return review.run_pipeline(self.ctx, agents, Path("."), Path("."), None, post=post)

    def test_the_first_reviewers_findings_are_posted_while_the_slow_one_is_still_running(self) -> None:
        self.round_decisions["decide.review-general"] = {"actions": [comment_on_line_one("fast")],
                                                         "overview": None, "dropped": []}
        self.run_pipeline()
        # The slow reviewer only returns after the first round was decided (see fake_run_agent), and the
        # first round was posted before the slow reviewer's own round.
        self.assertLess(self.events.index("round:review-general"), self.events.index(f"round:{self.slow}"))
        self.assertLess(self.events.index("post:1c/0r"), self.events.index(f"round:{self.slow}"))
        self.assertEqual(self.events[-2:], ["final-round", "post:0c/0r"])

    def test_a_findings_round_may_only_comment_or_reply(self) -> None:
        self.round_decisions["decide.review-general"] = {"actions": [
            comment_on_line_one("ok"), {"type": "resolve", "thread_id": "open", "body": "x"},
            {"type": "reopen", "thread_id": "open", "body": "x"}], "overview": "ignored", "dropped": []}
        outcome = self.run_pipeline()
        self.assertEqual((len(outcome.plan.new), outcome.plan.resolves, outcome.plan.reopens), (1, [], []))
        self.assertEqual(len([r for r in outcome.plan.rejected if "not allowed in this round" in r]), 2)

    def test_the_final_round_may_comment_for_a_problem_no_thread_can_carry(self) -> None:
        # A thread that cannot be reopened, or that a person closed, needs a new comment from the final round.
        self.final_decision = {"actions": [comment_on_line_one("late")], "overview": "x", "dropped": []}
        outcome = self.run_pipeline()
        self.assertEqual([f.title for f in outcome.plan.new], ["late"])

    def test_the_final_round_takes_findings_from_a_round_that_failed(self) -> None:
        self.broken = {"decide.review-general"}
        self.final_decision = {"actions": [comment_on_line_one("recovered")], "overview": "x", "dropped": []}
        outcome = self.run_pipeline()
        self.assertEqual([f.title for f in outcome.plan.new], ["recovered"])

    def test_the_final_round_resolves_threads(self) -> None:
        self.final_decision = {"actions": [{"type": "resolve", "thread_id": "open", "body": "fixed"}],
                               "overview": "x", "dropped": []}
        outcome = self.run_pipeline()
        self.assertEqual([r["thread_id"] for r in outcome.plan.resolves], ["open"])

    def test_the_same_problem_is_not_posted_by_two_rounds(self) -> None:
        for name in ("review-general", self.slow):
            self.round_decisions[f"decide.{name}"] = {"actions": [comment_on_line_one("Same problem")],
                                                       "overview": None, "dropped": []}
        outcome = self.run_pipeline()
        self.assertEqual(len(outcome.plan.new), 1)

    def test_inline_comments_across_rounds_stay_under_the_cap(self) -> None:
        for name in ("review-general", self.slow):
            self.round_decisions[f"decide.{name}"] = {"actions": [
                comment_on_line_one(f"{name} {i}", line=1 + (i % 4)) for i in range(review.MAX_INLINE_COMMENTS)],
                "overview": None, "dropped": []}
        outcome = self.run_pipeline()
        self.assertLessEqual(len(outcome.plan.new), review.MAX_INLINE_COMMENTS)
        self.assertGreaterEqual(len(outcome.plan.new) + len(outcome.plan.outside), review.MAX_INLINE_COMMENTS)

    def test_nothing_is_posted_without_post(self) -> None:
        self.round_decisions["decide.review-general"] = {"actions": [comment_on_line_one("x")],
                                                         "overview": None, "dropped": []}
        outcome = self.run_pipeline(post=False)
        self.assertFalse([e for e in self.events if e.startswith("post:")])
        self.assertEqual(len(outcome.plan.new), 1)

    def test_a_reviewer_with_no_findings_needs_no_round(self) -> None:
        self.finding = None
        self.run_pipeline()
        self.assertFalse([e for e in self.events if e.startswith("round:")])
        self.assertIn("final-round", self.events)

    def test_run_parallel_calls_the_hook_as_each_job_finishes(self) -> None:
        order: list[tuple[str, set[str]]] = []
        gate = threading.Event()

        def fast():
            return "fast"

        def slow():
            assert gate.wait(10)
            return "slow"

        def on_done(name, result, running):
            order.append((name, running))
            if name == "fast":
                gate.set()

        results, failures = review.run_parallel({"fast": fast, "slow": slow}, on_done=on_done)
        self.assertEqual((results, failures), ({"fast": "fast", "slow": "slow"}, {}))
        self.assertEqual(order, [("fast", {"slow"}), ("slow", set())])

    def test_a_failed_job_is_not_passed_to_the_hook(self) -> None:
        def boom():
            raise review.ReviewError("no")

        seen: list[str] = []
        _, failures = review.run_parallel({"a": boom, "b": lambda: 1}, on_done=lambda n, r, run: seen.append(n))
        self.assertEqual((seen, list(failures)), (["b"], ["a"]))

    def test_a_follow_up_on_a_thread_is_posted_once_across_rounds(self) -> None:
        reply = {"type": "reply", "thread_id": "open", "body": "again"}
        for name in ("review-general", self.slow):
            self.round_decisions[f"decide.{name}"] = {"actions": [dict(reply)], "overview": None, "dropped": []}
        self.final_decision = {"actions": [dict(reply)], "overview": "x", "dropped": []}
        outcome = self.run_pipeline()
        self.assertEqual([r["thread_id"] for r in outcome.plan.replies], ["open"])
        self.assertEqual(len([r for r in outcome.plan.rejected if "already got a follow-up" in r]), 2)

    def test_the_final_round_can_comment_when_a_thread_cannot_be_reopened(self) -> None:
        self.final_decision = {"actions": [comment_on_line_one("back again")], "overview": "x", "dropped": []}
        outcome = self.run_pipeline()
        self.assertEqual([f.title for f in outcome.plan.new], ["back again"])

    def test_a_problem_a_findings_round_posted_is_not_posted_again_by_the_final_round(self) -> None:
        self.round_decisions["decide.review-general"] = {"actions": [comment_on_line_one("Same")],
                                                         "overview": None, "dropped": []}
        self.final_decision = {"actions": [comment_on_line_one("same")], "overview": "x", "dropped": []}
        self.assertEqual(len(self.run_pipeline().plan.new), 1)

    def test_the_incomplete_banner_clears_when_the_final_round_recovers_the_findings(self) -> None:
        self.broken = {"decide.review-general"}
        self.final_decision = {"actions": [comment_on_line_one("recovered")], "overview": "x", "dropped": []}
        outcome = self.run_pipeline()
        self.assertNotIn("decide/review-general", outcome.failed)

    def test_the_banner_stays_if_the_final_round_failed_too(self) -> None:
        self.broken = {"decide.review-general", "decide"}
        outcome = self.run_pipeline()
        self.assertIn("decide/review-general", outcome.failed)

    def test_this_runs_own_comments_are_not_counted_as_earlier_threads(self) -> None:
        # Live threads after a round include the one this run just posted; they must not become "open".
        own = thread("own", body=f"{render.MARKER}\n🟠 **Major · Safety** — Posted by this run")
        ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha="abc",
                             threads=[thread("old", comment_id=1)], earlier_thread_ids=frozenset({"old"}))
        gh = FakeGh(responses={"pr view": json.dumps({"headRefOid": "abc"})})
        with mock.patch.object(review, "gh", gh), mock.patch.object(
                review, "fetch_threads", return_value=[thread("old", comment_id=1), own]):
            review.apply_plan(review.Plan(), ctx)
        self.assertEqual([t["thread_id"] for t in ctx.threads], ["old"])

    def test_this_runs_own_comments_are_left_out_even_when_there_were_no_earlier_threads(self) -> None:
        own = thread("own")
        ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha="abc",
                             threads=[], earlier_thread_ids=frozenset())
        gh = FakeGh(responses={"pr view": json.dumps({"headRefOid": "abc"})})
        with mock.patch.object(review, "gh", gh), mock.patch.object(review, "fetch_threads", return_value=[own]):
            review.apply_plan(review.Plan(), ctx)
        self.assertEqual(ctx.threads, [])

    def test_without_a_record_of_earlier_threads_all_live_threads_are_used(self) -> None:
        ctx = review.Context(description="d", title="t", files=[], diff=DIFF, pr_number=7, head_sha="abc")
        gh = FakeGh(responses={"pr view": json.dumps({"headRefOid": "abc"})})
        with mock.patch.object(review, "gh", gh), mock.patch.object(
                review, "fetch_threads", return_value=[thread("a"), thread("b")]):
            review.apply_plan(review.Plan(), ctx)
        self.assertEqual(len(ctx.threads), 2)

    def test_the_pipeline_records_the_threads_it_started_with(self) -> None:
        self.run_pipeline(post=False)
        self.assertEqual(self.ctx.earlier_thread_ids, frozenset({"open"}))

    def test_a_comment_posted_by_an_earlier_round_is_not_shown_as_open_from_earlier(self) -> None:
        """End to end through the real apply_plan: the live threads include the comment a round just posted."""
        self.round_decisions["decide.review-general"] = {"actions": [comment_on_line_one("Brand new")],
                                                         "overview": None, "dropped": []}
        own = thread("own", comment_id=99, body=f"{render.MARKER}\n🟠 **Major · Safety** — Brand new")
        prompts: list[str] = []
        orig = self.fake_run_agent

        def spy(agent, prompt, *args, **kwargs):
            if (kwargs.get("label") or agent.name) == "decide":
                prompts.append(prompt)
            return orig(agent, prompt, *args, **kwargs)

        agents = [a for a in self.agents if a.stage in ("triage", "decide")]
        general = next(a for a in self.agents if a.name == "review-general")
        agents += [general, dataclasses_replace(general, name=self.slow)]
        gh = FakeGh(responses={"pr view": json.dumps({"headRefOid": "abc"})})
        live = [thread("open", comment_id=11), own]  # what GitHub returns once the first round has posted
        with mock.patch.object(review, "run_agent", spy), mock.patch.object(review, "gh", gh), mock.patch.object(
                review, "fetch_threads", return_value=live):
            outcome = review.run_pipeline(self.ctx, agents, Path("."), Path("."), None, post=True)
        self.assertEqual([t["thread_id"] for t in self.ctx.threads], ["open"])
        summary = review.build_summary(outcome, outcome.plan, self.ctx)
        self.assertIn("Brand new", summary)  # listed once, under "New in this review"
        self.assertEqual(summary.count("Brand new"), 1)
        self.assertNotIn('"thread_id": "own"', prompts[-1])  # and the final decider is not asked about it

    def test_the_decider_uses_a_fast_model_and_the_judging_agents_do_not(self) -> None:
        models = {a.name: a.model for a in self.agents}
        self.assertEqual(models["decide"], "sonnet")
        self.assertEqual(models["council-chair"], "opus")  # it checks other models' claims against the code


if __name__ == "__main__":
    unittest.main()
