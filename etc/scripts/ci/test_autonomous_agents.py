#!/usr/bin/env python3
"""Unit tests for `autonomous_agents.py`. Run with `python3 etc/scripts/ci/test_autonomous_agents.py`."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from typing import Any

import autonomous_agents as agents

INDEX = agents.load_docs_index()
SPEC = agents.AgentSpec("docs-index", "agent:docs-index", "x.md", "agent/docs-index-", "claude-sonnet-5-5", 1, 1)


def entry(path: str, summary: str = "Covers a thing.", digest: str = "0000000000") -> Any:
    return INDEX.Entry(path, summary, digest)


def manifest(*entries: Any) -> str:
    return INDEX.render_manifest(list(entries))


class FakeGitHub:
    """Serve canned `gh pr list` output and record mutations."""

    def __init__(self, pulls: list[dict[str, Any]]) -> None:
        self.pulls, self.calls = pulls, []

    def json(self, args: list[str]) -> Any:
        return self.pulls

    def run(self, args: list[str]) -> str:
        self.calls.append(args)
        return ""

    def api(self, method: str, endpoint: str, payload: dict[str, Any] | None = None) -> Any:
        raise AssertionError("unexpected API call")


class RegistryTests(unittest.TestCase):
    def write(self, **overrides: Any) -> Path:
        agent = {
            "label": "agent:x", "instructions": "x.md", "branch_prefix": "agent/x-",
            "model": "claude-sonnet-5-5", "min_open_prs": 1, "max_open_prs": 2,
        }  # fmt: skip
        agent.update(overrides)
        path = Path(tempfile.mkdtemp()) / "registry.json"
        path.write_text(json.dumps({"target": {"repository": "a/b", "base_branch": "main"}, "agents": {"x": agent}}))
        return path

    def test_committed_registry_is_valid_and_defines_docs_index(self) -> None:
        target, loaded = agents.load_registry()
        self.assertEqual(target["repository"], "base/base")
        spec = loaded[agents.AGENT]
        self.assertEqual((spec.min_open_prs, spec.max_open_prs), (1, 1))
        self.assertTrue((agents.ROOT / spec.instructions).is_file())

    def test_bounds_and_model_come_from_the_registry(self) -> None:
        _, loaded = agents.load_registry(self.write(model="claude-opus-5-5", min_open_prs=0, max_open_prs=3))
        self.assertEqual((loaded["x"].model, loaded["x"].min_open_prs, loaded["x"].max_open_prs), ("claude-opus-5-5", 0, 3))

    def test_rejects_bad_definitions(self) -> None:
        for override in (
            {"model": "gpt-5"},
            {"branch_prefix": "x-"},
            {"branch_prefix": "agent/x"},
            {"min_open_prs": 2, "max_open_prs": 1},
            {"max_open_prs": 0, "min_open_prs": 0},
            {"min_open_prs": "1"},
            {"min_open_prs": True},
        ):
            with self.subTest(override=override), self.assertRaises(agents.AgentError):
                agents.load_registry(self.write(**override))


class PlanTests(unittest.TestCase):
    def test_open_prs_are_filtered_by_prefix_and_sorted(self) -> None:
        github = FakeGitHub(
            [
                {"number": 9, "headRefName": "agent/docs-index-2"},
                {"number": 3, "headRefName": "someone/else"},
                {"number": 4, "headRefName": "agent/docs-index-1"},
            ]
        )
        found = agents.open_agent_prs(github, {"repository": "a/b", "base_branch": "main"}, SPEC)
        self.assertEqual([pull["pull_request"] for pull in found], [4, 9])

    def test_creates_the_minimum_when_work_exists_and_none_is_open(self) -> None:
        plan = agents.plan_agent(SPEC, [], needed=True)
        self.assertEqual(plan, {"include": [{"mode": "create", "pull_request": 0, "branch": ""}], "close": []})

    def test_refreshes_the_oldest_and_closes_the_excess(self) -> None:
        open_prs = [{"pull_request": 4, "branch": "agent/docs-index-a"}, {"pull_request": 9, "branch": "agent/docs-index-b"}]
        plan = agents.plan_agent(SPEC, open_prs, needed=True)
        self.assertEqual(plan["include"], [{"mode": "refresh", **open_prs[0]}])
        self.assertEqual(plan["close"], [open_prs[1]])

    def test_closes_everything_when_the_index_is_current(self) -> None:
        open_prs = [{"pull_request": 4, "branch": "agent/docs-index-a"}]
        self.assertEqual(agents.plan_agent(SPEC, open_prs, needed=False), {"include": [], "close": open_prs})
        self.assertEqual(agents.plan_agent(SPEC, [], needed=False), {"include": [], "close": []})

    def test_minimum_above_one_opens_several(self) -> None:
        spec = agents.AgentSpec("x", "l", "i", "agent/x-", "claude-sonnet-5-5", 2, 3)
        plan = agents.plan_agent(spec, [{"pull_request": 1, "branch": "agent/x-1"}], needed=True)
        self.assertEqual([item["mode"] for item in plan["include"]], ["refresh", "create"])

    def test_close_only_touches_still_open_agent_pull_requests(self) -> None:
        github = FakeGitHub([{"number": 4, "headRefName": "agent/docs-index-a"}])
        closing = [
            {"pull_request": 4, "branch": "agent/docs-index-a"},
            {"pull_request": 5, "branch": "agent/docs-index-b"},
            {"pull_request": 4, "branch": "agent/docs-index-other"},
        ]
        agents.command_close(github, agents.AGENT, closing)
        self.assertEqual([call[2] for call in github.calls if call[:2] == ["pr", "close"]], ["4"])


class PrepareTests(unittest.TestCase):
    docs = {"a.md": "aaaaaaaaaa", "b.md": "bbbbbbbbbb", "c.md": "cccccccccc"}

    def prepare(self, entries: list[Any], previous: list[Any] | None = None) -> tuple[list[Any], Any]:
        return agents.prepare_entries(INDEX, entries, self.docs, previous)

    def test_new_and_changed_docs_are_separated_and_dead_entries_dropped(self) -> None:
        entries = [entry("a.md", digest="aaaaaaaaaa"), entry("b.md", digest="old"), entry("gone.md")]
        merged, work = self.prepare(entries)
        self.assertEqual([e.path for e in merged], ["a.md", "b.md", "c.md"])
        self.assertEqual((work.new, work.changed), (["c.md"], ["b.md"]))

    def test_overlong_summaries_are_asked_about_again(self) -> None:
        _, work = self.prepare([entry("a.md", "x" * 201, "aaaaaaaaaa"), entry("b.md", digest="bbbbbbbbbb"), entry("c.md", digest="cccccccccc")])
        self.assertEqual(work.changed, ["a.md"])

    def test_previous_summaries_are_reused_only_for_the_current_doc_content(self) -> None:
        entries = [entry("a.md", digest="old"), entry("b.md", digest="old"), entry("c.md", digest="cccccccccc")]
        previous = [
            entry("a.md", "Reviewed summary.", "aaaaaaaaaa"),
            entry("b.md", "Reviewed against older content.", "older"),
        ]
        merged, work = self.prepare(entries, previous)
        summaries = {e.path: (e.summary, e.digest) for e in merged}
        self.assertEqual(summaries["a.md"], ("Reviewed summary.", "aaaaaaaaaa"))
        self.assertEqual(summaries["b.md"][1], "old")
        self.assertEqual((work.new, work.changed), ([], ["b.md"]))

    def test_previous_placeholders_are_never_reused(self) -> None:
        previous = [entry("c.md", INDEX.PLACEHOLDER, "cccccccccc")]
        _, work = self.prepare([entry("a.md", digest="aaaaaaaaaa"), entry("b.md", digest="bbbbbbbbbb")], previous)
        self.assertEqual(work.new, ["c.md"])


class ValidationTests(unittest.TestCase):
    work = agents.Work(new=["new.md"], changed=["old.md"])
    baseline = manifest(
        entry("new.md", INDEX.PLACEHOLDER, "1111111111"),
        entry("old.md", "Old summary.", "2222222222"),
        entry("other.md", "Other summary.", "3333333333"),
    )

    def check(self, *entries: Any) -> list[str]:
        return agents.validate_summaries(INDEX, self.work, self.baseline, manifest(*entries))

    def good(self, **overrides: str) -> list[Any]:
        values = {"new.md": "Written summary.", "old.md": "Old summary.", "other.md": "Other summary."}
        values.update(overrides)
        digests = {"new.md": "1111111111", "old.md": "2222222222", "other.md": "3333333333"}
        return [entry(path, summary, digests[path]) for path, summary in values.items()]

    def test_accepts_a_summary_for_the_new_doc_and_an_untouched_changed_doc(self) -> None:
        self.assertEqual(self.check(*self.good()), [])

    def test_accepts_a_rewritten_changed_doc(self) -> None:
        self.assertEqual(self.check(*self.good(**{"old.md": "Rewritten."})), [])

    def test_rejects_a_remaining_placeholder(self) -> None:
        errors = self.check(*self.good(**{"new.md": INDEX.PLACEHOLDER}))
        self.assertEqual(errors, ["new.md: replace the TODO summary"])

    def test_rejects_edits_to_unlisted_docs(self) -> None:
        errors = self.check(*self.good(**{"other.md": "Sneaky."}))
        self.assertEqual(errors, ["other.md: not a listed doc; restore its summary"])

    def test_rejects_digest_edits(self) -> None:
        entries = self.good()
        entries[1] = entry("old.md", "Old summary.", "9999999999")
        self.assertEqual(self.check(*entries), ["old.md: do not edit `digest`; the workflow stamps it"])

    def test_rejects_structure_changes(self) -> None:
        self.assertEqual(self.check(*self.good()[:2]), ["do not add, remove, rename, or reorder manifest entries"])

    def test_rejects_long_multiline_and_empty_summaries(self) -> None:
        self.assertIn("201 characters", self.check(*self.good(**{"new.md": "x" * 201}))[0])
        self.assertIn("one non-empty line", self.check(*self.good(**{"new.md": "a\nb"}))[0])
        self.assertIn("one non-empty line", self.check(*self.good(**{"old.md": "  "}))[0])

    def test_rejects_an_invalid_manifest(self) -> None:
        errors = agents.validate_summaries(INDEX, self.work, self.baseline, "not = [toml")
        self.assertIn("not a valid manifest", errors[0])


class ProcessTests(unittest.TestCase):
    def test_outputs_survive_newlines_and_quotes(self) -> None:
        path = Path(tempfile.mkdtemp()) / "out"
        agents.write_github_outputs(path, {"a": "x\ny=z", "b": {"k": [1]}})
        lines = path.read_text().splitlines()
        self.assertTrue(lines[0].startswith("a<<ghadelim_"))
        self.assertEqual(lines[1:3], ["x", "y=z"])
        self.assertEqual(lines[5], '{"k":[1]}')

    def test_a_failing_stream_write_kills_the_child_and_closes_the_log_group(self) -> None:
        import io
        import os
        import stat
        import unittest.mock as mock

        workdir = Path(tempfile.mkdtemp())
        pid_file = workdir / "pid"
        script = workdir / "claude"
        script.write_text(f"#!/bin/sh\necho $$ > {pid_file}\necho '{{}}'\nexec sleep 60\n")
        script.chmod(script.stat().st_mode | stat.S_IEXEC)
        stream = Path(tempfile.mkdtemp()) / "stream.jsonl"
        out = io.StringIO()
        with mock.patch("sys.stdout", out), mock.patch.object(Path, "open", side_effect=_broken_open(stream)):
            with self.assertRaises(OSError):
                agents.run_claude(script, "claude-sonnet-5-5", "p", stream)
        self.assertRegex(out.getvalue(), r"::stop-commands::(agent-[0-9a-f-]+)\n::\1::")
        with self.assertRaises(ProcessLookupError):
            os.kill(int(pid_file.read_text()), 0)

    def test_top_level_handler_reports_malformed_api_responses(self) -> None:
        import unittest.mock as mock

        with mock.patch.object(agents, "command_plan", side_effect=IndexError("parents")):
            self.assertEqual(agents.main(["plan"]), 1)


def _broken_open(stream: Path):
    class Broken:
        def __enter__(self):
            return self

        def __exit__(self, *_):
            return False

        def write(self, _):
            raise OSError("disk full")

    return lambda *_a, **_k: Broken()


class RenderTests(unittest.TestCase):
    def test_pr_body_lists_each_group_and_omits_empty_ones(self) -> None:
        body = agents.render_pr_body({"added": ["a.md"], "updated": [], "confirmed": ["b.md"]}, SPEC)
        self.assertIn("### New docs summarized\n- `a.md`", body)
        self.assertIn("### Changed docs re-read, summary still accurate\n- `b.md`", body)
        self.assertNotIn("Summaries rewritten", body)
        self.assertIn("never conflicts", body)

    def test_stream_rendering_hides_tool_output(self) -> None:
        lines = agents.render_claude_event(
            {"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Read", "input": {"file_path": "a.md"}}]}}
        )
        self.assertEqual(lines, ["[tool] Read a.md"])
        result = {"type": "user", "message": {"content": [{"type": "tool_result", "content": "SECRET", "is_error": True}]}}
        self.assertEqual(agents.render_claude_event(result), ["[tool-result] error"])

    def test_prompt_lists_docs_and_carries_repair_feedback(self) -> None:
        prompt = agents.build_prompt(SPEC, agents.Work(["n.md"], ["c.md"]), "ERROR: bad")
        self.assertIn("- n.md", prompt)
        self.assertIn("- c.md", prompt)
        self.assertIn("ERROR: bad", prompt)

    def test_the_model_can_only_edit_the_manifest(self) -> None:
        self.assertIn("Edit(./etc/docs-index.toml)", agents.CLAUDE_ALLOWED)
        self.assertNotIn("Write", agents.CLAUDE_TOOLS)
        self.assertNotIn("Bash", agents.CLAUDE_TOOLS)


if __name__ == "__main__":
    unittest.main()
