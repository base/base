#!/usr/bin/env python3
"""Unit tests for review.py and render.py. Run with `python3 .agents/skills/pr-review/test_review.py`."""

import json
import os
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

    def test_thread_header_reads_the_first_versions_plain_label(self) -> None:
        body = f"{render.MARKER}\n**Major:** This overlay does not stop a PR. Second sentence."
        severity, text = render.thread_header(thread("t", body=body))
        self.assertEqual((severity, text), ("major", "🟠 **Major** — This overlay does not stop a PR."))

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

    def test_reply_and_unresolve(self) -> None:
        plan = self.plan({"type": "reply", "thread_id": "open", "body": "more"},
                         {"type": "unresolve", "thread_id": "done", "body": "still broken"})
        self.assertEqual([r["thread_id"] for r in plan.replies], ["open"])
        self.assertIn("**Follow-up:** more", plan.replies[0]["body"])
        self.assertEqual([r["thread_id"] for r in plan.unresolves], ["done"])
        self.assertIn("**Reopened:** still broken", plan.unresolves[0]["body"])
        self.assertEqual(plan.rejected, [])

    def test_resolve_closes_an_open_bot_thread_only(self) -> None:
        plan = self.plan({"type": "resolve", "thread_id": "open", "body": "fixed"},
                         {"type": "resolve", "thread_id": "done", "body": "x"},
                         {"type": "resolve", "thread_id": "human", "body": "x"})
        self.assertEqual([r["thread_id"] for r in plan.resolves], ["open"])
        self.assertIn("**Fixed:** fixed", plan.resolves[0]["body"])
        self.assertEqual(len(plan.rejected), 2)

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
            reopened={u["thread_id"] for u in plan.unresolves}, fixed={r["thread_id"] for r in plan.resolves},
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
        plan = review.build_plan({"actions": [{"type": "unresolve", "thread_id": "done", "body": "again"}]},
                                 [thread("done", resolved=True)], set())
        text = self.summary(plan, [thread("done", resolved=True)])
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
            return {"url": url, "body": text, "author": {"login": who, "__typename": kind}}

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


class BudgetTests(unittest.TestCase):
    def agent(self, timeout: int = 1000) -> review.Agent:
        return review.Agent("a", "review", ("always",), "m", "high", "Read", timeout, None, "p", Path("a.md"))

    def test_timeout_shrinks_to_leave_time_for_later_stages(self) -> None:
        budget = review.Budget(2000)
        self.assertEqual(budget.timeout_for(1000, "a"), 1000)
        self.assertLessEqual(budget.timeout_for(1000, "a", reserve=1500), 500)

    def test_stage_is_skipped_when_the_budget_is_spent(self) -> None:
        with self.assertRaises(review.ReviewError):
            review.Budget(100).timeout_for(1000, "a", reserve=90)

    def test_worst_case_of_the_checked_in_agents_fits_the_ci_job(self) -> None:
        agents = {a.name: a for a in review.load_agents()}
        members = [a for a in agents.values() if a.stage == "council"]
        worst = (agents["triage"].timeout_seconds + max(m.timeout_seconds for m in members)
                 + review.VOTE_TIMEOUT_SECONDS + agents["council-chair"].timeout_seconds
                 + agents["decide"].timeout_seconds)
        job_limit = 90 * 60
        # Leave ten minutes of the CI job to post the results and upload artifacts.
        self.assertLessEqual(review.DEFAULT_BUDGET_SECONDS, job_limit - 600)
        # With the budget, a worst-case run still ends with a decision; without it, it would not.
        self.assertGreater(worst, review.MIN_STAGE_SECONDS)

    def test_agents_do_not_inherit_github_credentials(self) -> None:
        with mock.patch.dict(os.environ, {"GH_TOKEN": "x", "GITHUB_TOKEN": "y", "ANTHROPIC_API_KEY": "z"}):
            env = review.agent_env()
        self.assertEqual((env.get("GH_TOKEN"), env.get("GITHUB_TOKEN"), env.get("ANTHROPIC_API_KEY")),
                         (None, None, "z"))


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
        self.ctx = review.Context(description="d", title="t", files=["src/a.rs"], diff=DIFF, pr_number=7,
                                  head_sha="abc", threads=[thread("open"), thread("done", resolved=True)])
        decision = {"actions": [
            comment(), {"type": "reply", "thread_id": "open", "body": "more"},
            {"type": "resolve", "thread_id": "open", "body": "fixed in `run`"},
            {"type": "unresolve", "thread_id": "done", "body": "still broken"}]}
        self.plan = review.build_plan(decision, self.ctx.threads, review.diff_new_lines(DIFF))

    def apply(self, gh: FakeGh) -> list[str]:
        def summarize(plan: review.Plan) -> str | None:
            return render.render_summary(
                overview=None, new=plan.new, outside=plan.outside, threads=self.ctx.threads,
                reopened={u["thread_id"] for u in plan.unresolves}, fixed={r["thread_id"] for r in plan.resolves},
                failed=[], details="d", repo="base/base", head_sha="abc", replace_existing=True)

        with mock.patch.object(review, "gh", gh):
            return review.apply_plan(self.plan, self.ctx, summarize)

    def test_everything_is_posted_and_the_old_summary_is_replaced_after_the_new_one(self) -> None:
        gh = FakeGh(responses={"issues/7/comments": "11\n12\n"})
        self.assertEqual(self.apply(gh), [])
        self.assertEqual(len(gh.matching("pulls/7/reviews")), 1)
        self.assertEqual(len(gh.matching("unresolveReviewThread")), 1)
        self.assertEqual(len(gh.matching("{ resolveReviewThread(")), 1)
        comment_calls = [i for i, c in enumerate(gh.calls) if c[:2] == ["pr", "comment"]]
        deletes = [i for i, c in enumerate(gh.calls) if "DELETE" in c]
        self.assertEqual(len(comment_calls), 1)
        self.assertEqual(len(deletes), 2)
        self.assertLess(comment_calls[0], min(deletes))

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
        [summary] = [i for c, i in zip(gh.calls, gh.inputs) if c[:2] == ["pr", "comment"]]
        self.assertIn("### Outside the diff", summary)
        self.assertIn("Panics on empty batch", summary)

    def test_a_failed_resolve_or_reopen_is_not_reported_as_applied(self) -> None:
        gh = FakeGh(fail=("{ resolveReviewThread(", "{ unresolveReviewThread("))
        problems = self.apply(gh)
        self.assertEqual(len(problems), 2)
        self.assertEqual((self.plan.resolves, self.plan.unresolves), ([], []))
        # No "Fixed" or "Reopened" reply for a change that did not happen.
        replies = [" ".join(c) for c in gh.calls if "addPullRequestReviewThreadReply" in " ".join(c)]
        self.assertEqual(len(replies), 1)
        self.assertNotIn("Fixed", replies[0])
        [summary] = [i for c, i in zip(gh.calls, gh.inputs) if c[:2] == ["pr", "comment"]]
        self.assertNotIn("Fixed in this push", summary)
        self.assertNotIn("### Reopened", summary)

    def test_state_changes_come_before_their_explanations(self) -> None:
        gh = FakeGh()
        self.apply(gh)
        text = [" ".join(c) for c in gh.calls]
        reply = next(i for i, c in enumerate(text) if "Fixed" in c)
        resolve = next(i for i, c in enumerate(text) if "{ resolveReviewThread(" in c)
        self.assertLess(resolve, reply)

    def test_a_failed_reply_does_not_stop_the_summary(self) -> None:
        gh = FakeGh(fail=("addPullRequestReviewThreadReply",))
        problems = self.apply(gh)
        self.assertEqual(len(problems), 3)
        self.assertEqual(len(gh.matching("pulls/7/reviews")), 1)
        self.assertEqual(len([c for c in gh.calls if c[:2] == ["pr", "comment"]]), 1)

    def test_a_failed_summary_post_keeps_the_old_summary(self) -> None:
        gh = FakeGh(fail=("pr comment",), responses={"issues/7/comments": "11\n"})
        with self.assertRaises(review.ReviewError):
            self.apply(gh)
        self.assertEqual(gh.matching("DELETE"), [])


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
        self.failing = {"council-tests"}
        outcome = self.run_pipeline()
        self.assertIn("council/council-tests", outcome.failed)
        self.assertIn("council-chair", self.calls)

    def test_every_reviewer_failing_is_an_error(self) -> None:
        self.failing = {"review-general", "council-invariants", "council-adversary", "council-tests"}
        with self.assertRaises(review.ReviewError):
            self.run_pipeline()


if __name__ == "__main__":
    unittest.main()
