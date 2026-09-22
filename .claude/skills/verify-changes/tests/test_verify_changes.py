"""Black-box tests for the parallel verify-changes protocol."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SKILL_DIR = Path(__file__).resolve().parents[1]
CLI = SKILL_DIR / "verify_changes.py"
REGISTRY = SKILL_DIR / "agents.json"

FAKE_AGENT = r"""
import json
import os
from pathlib import Path

agent_id = os.environ["VERIFY_CHANGES_AGENT_ID"]
mode = os.environ.get("FAKE_MODE", "parallel")
if agent_id == "initial-decider":
    if mode == "unknown":
        output = {"summary": "bad", "checks": [{"id": "not-registered", "task": "bad", "required": True}]}
    elif mode == "target-only":
        output = {"summary": "targeted tests", "checks": [{"id": "targeted-tests", "task": "run affected tests", "required": True}]}
    else:
        output = {
            "summary": "Rust behavior and tests both apply",
            "checks": [
                {"id": "rust-correctness", "task": "review public behavior", "required": True},
                {"id": "targeted-tests", "task": "run focused tests", "required": True}
            ]
        }
    print(json.dumps(output))
elif agent_id.endswith("-decider"):
    if mode == "skip":
        print(json.dumps({"summary": "not applicable", "run_agent": False}))
    else:
        print(json.dumps({"summary": "worker is needed", "run_agent": True, "agent_task": "inspect and test this change"}))
else:
    if not Path("local-change.txt").exists():
        print(json.dumps({"status": "fail", "summary": "source local patch was not applied", "findings": [{"blocking": True}], "commands_run": []}))
    else:
        Path(f"agent-note-{agent_id}.txt").write_text("isolated agent edit\n")
        print(json.dumps({"status": "pass", "summary": "worker completed", "findings": [], "commands_run": ["fake test command"]}))
"""


class VerifyChangesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.worktree = Path(self.temporary_directory.name) / "repo"
        self.worktree.mkdir()
        self.fake_agent = Path(self.temporary_directory.name) / "fake_agent.py"
        self.fake_agent.write_text(FAKE_AGENT)
        self.git("init")
        self.git("config", "user.email", "verify@example.test")
        self.git("config", "user.name", "Verify Test")
        (self.worktree / "changed.rs").write_text("pub fn old() {}\n")
        self.git("add", "changed.rs")
        self.git("commit", "-m", "initial")
        (self.worktree / "changed.rs").write_text("pub fn new() {}\n")
        self.git("commit", "-am", "change")
        (self.worktree / "local-change.txt").write_text("apply me to workers\n")

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def git(self, *args: str) -> None:
        subprocess.run(["git", "-C", str(self.worktree), *args], check=True, capture_output=True, text=True)

    def run_cli(self, extra_env: dict[str, str] | None = None, *args: str) -> subprocess.CompletedProcess[str]:
        command = f"{sys.executable} {self.fake_agent}"
        environment = os.environ.copy()
        environment.update(
            {
                "VERIFY_CHANGES_DECIDER_COMMAND": command,
                "VERIFY_CHANGES_RUST_CORRECTNESS_DECIDER_COMMAND": command,
                "VERIFY_CHANGES_RUST_CORRECTNESS_AGENT_COMMAND": command,
                "VERIFY_CHANGES_TARGETED_TESTS_DECIDER_COMMAND": command,
                "VERIFY_CHANGES_TARGETED_TESTS_AGENT_COMMAND": command,
            }
        )
        if extra_env:
            environment.update(extra_env)
        return subprocess.run(
            [
                sys.executable,
                str(CLI),
                "--worktree",
                str(self.worktree),
                "--registry",
                str(REGISTRY),
                "--artifacts-dir",
                ".artifacts",
                "--max-parallel",
                "2",
                *args,
            ],
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )

    def test_parallel_checks_receive_local_patch_and_capture_isolated_edits(self) -> None:
        completed = self.run_cli()

        self.assertEqual(completed.returncode, 0, completed.stderr)
        report = json.loads(completed.stdout)
        self.assertTrue(report["passed"])
        self.assertEqual([result["id"] for result in report["results"]], ["rust-correctness", "targeted-tests"])
        for check_id in ("rust-correctness", "targeted-tests"):
            patch = self.worktree / ".artifacts" / "checks" / check_id / "agent.patch"
            self.assertTrue(patch.exists())
            patch_text = patch.read_text()
            self.assertIn(f"agent-note-{check_id}.txt", patch_text)
            self.assertNotIn("local-change.txt", patch_text)
        self.assertFalse((self.worktree / "agent-note-rust-correctness.txt").exists())
        self.assertEqual(self.git_output("worktree", "list").count(str(self.worktree)), 1)

    def test_registry_includes_cli_and_decider_contract_checks(self) -> None:
        registry = json.loads(REGISTRY.read_text())
        check_ids = {check["id"] for check in registry["checks"]}

        self.assertTrue({"verify-changes-cli", "decider-contracts", "long-term-simplicity"}.issubset(check_ids))
        self.assertEqual(registry["checks"][0]["id"], "long-term-simplicity")

    def test_initial_decider_cannot_select_unknown_check(self) -> None:
        completed = self.run_cli({"FAKE_MODE": "unknown"})

        self.assertEqual(completed.returncode, 2)
        report = json.loads((self.worktree / ".artifacts" / "report.json").read_text())
        self.assertIn("unknown check", report["error"])

    def test_missing_required_check_agent_blocks_review(self) -> None:
        completed = self.run_cli({"FAKE_MODE": "target-only", "VERIFY_CHANGES_TARGETED_TESTS_AGENT_COMMAND": ""})

        self.assertEqual(completed.returncode, 1, completed.stderr)
        report = json.loads(completed.stdout)
        self.assertFalse(report["passed"])
        self.assertEqual(report["results"][0]["status"], "error")
        self.assertIn("missing check agent command", report["results"][0]["summary"])

    def test_check_decider_can_skip_its_worker(self) -> None:
        completed = self.run_cli({"FAKE_MODE": "skip"})

        self.assertEqual(completed.returncode, 0, completed.stderr)
        report = json.loads(completed.stdout)
        self.assertTrue(all(result["status"] == "skipped" for result in report["results"]))

    def test_dry_run_only_invokes_initial_decider(self) -> None:
        completed = self.run_cli({}, "--dry-run")

        self.assertEqual(completed.returncode, 0, completed.stderr)
        report = json.loads(completed.stdout)
        self.assertEqual(report["results"], [])
        self.assertFalse((self.worktree / ".artifacts" / "checks").exists())

    def git_output(self, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(self.worktree), *args], check=True, capture_output=True, text=True
        ).stdout


if __name__ == "__main__":
    unittest.main()
