#!/usr/bin/env python3
"""Route a Git change through an initial decider and parallel isolated review checks."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
DEFAULT_REGISTRY = SCRIPT_DIR / "agents.json"
VALID_AGENT_STATUSES = frozenset({"pass", "needs_changes", "fail", "error"})
WORKTREE_LOCK = threading.Lock()


class VerificationError(Exception):
    """Raised when the verification protocol cannot be completed safely."""


@dataclass(frozen=True)
class CheckRequest:
    """A validated check selected by the initial decider."""

    id: str
    task: str
    required: bool


def write_json(path: Path, value: Any) -> None:
    """Write formatted JSON, creating parent directories as needed."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def load_json(path: Path, description: str) -> dict[str, Any]:
    """Load an object-shaped JSON document."""
    try:
        value = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise VerificationError(f"could not read {description} at {path}: {error}") from error
    if not isinstance(value, dict):
        raise VerificationError(f"{description} at {path} must be a JSON object")
    return value


def parse_json_object(text: str, description: str) -> dict[str, Any]:
    """Parse exactly one JSON object emitted by an external agent."""
    try:
        value = json.loads(text)
    except json.JSONDecodeError as error:
        raise VerificationError(f"{description} did not emit valid JSON: {error}") from error
    if not isinstance(value, dict):
        raise VerificationError(f"{description} must emit a JSON object")
    return value


def run_git(worktree: Path, *args: str, input_text: str | None = None, check: bool = True) -> str:
    """Run Git in a worktree and return standard output."""
    result = subprocess.run(
        ["git", "-C", str(worktree), *args],
        check=False,
        capture_output=True,
        text=True,
        input=input_text,
    )
    if check and result.returncode:
        raise VerificationError(
            f"git {' '.join(args)} failed: {result.stderr.strip() or result.stdout.strip()}"
        )
    return result.stdout


def resolve_ref(worktree: Path, ref: str) -> str:
    """Resolve one user-supplied Git ref to a full object ID."""
    return run_git(worktree, "rev-parse", "--verify", f"{ref}^{{commit}}").strip()


def untracked_binary_patch(
    worktree: Path, excluded_prefixes: tuple[Path, ...] = ()
) -> str:
    """Create a binary patch that adds untracked files outside excluded prefixes."""
    patches: list[str] = []
    for relative_path in run_git(worktree, "ls-files", "--others", "--exclude-standard").splitlines():
        relative = Path(relative_path)
        if any(relative == prefix or prefix in relative.parents for prefix in excluded_prefixes):
            continue
        result = subprocess.run(
            ["git", "-C", str(worktree), "diff", "--binary", "--no-index", "/dev/null", relative_path],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode not in (0, 1):
            raise VerificationError(result.stderr.strip() or f"could not create patch for {relative_path}")
        patches.append(result.stdout)
    return "".join(patches)


def git_context(
    worktree: Path, base: str, head: str, excluded_untracked_prefixes: tuple[Path, ...] = ()
) -> dict[str, Any]:
    """Build review context from committed, staged, and untracked local changes."""
    base_sha = resolve_ref(worktree, base)
    head_sha = resolve_ref(worktree, head)
    return {
        "base": {"ref": base, "sha": base_sha},
        "head": {"ref": head, "sha": head_sha},
        "committed_changed_files": run_git(
            worktree, "diff", "--name-only", f"{base_sha}...{head_sha}"
        ).splitlines(),
        "working_changed_files": run_git(worktree, "diff", "--name-only").splitlines(),
        "staged_changed_files": run_git(worktree, "diff", "--cached", "--name-only").splitlines(),
        "untracked_files": run_git(worktree, "ls-files", "--others", "--exclude-standard").splitlines(),
        "committed_diff": run_git(worktree, "diff", "--binary", f"{base_sha}...{head_sha}"),
        "working_diff": run_git(worktree, "diff", "--binary"),
        "staged_diff": run_git(worktree, "diff", "--cached", "--binary"),
        "local_patch_from_head": run_git(worktree, "diff", "--binary", head_sha)
        + untracked_binary_patch(worktree, excluded_untracked_prefixes),
        "initial_status": run_git(worktree, "status", "--short"),
    }


def load_registry(path: Path) -> dict[str, dict[str, Any]]:
    """Load the check registry and validate its execution contract."""
    registry = load_json(path, "check registry")
    if registry.get("schema_version") != 2:
        raise VerificationError("check registry must have schema_version 2")
    checks = registry.get("checks")
    if not isinstance(checks, list):
        raise VerificationError("check registry must contain a checks array")

    by_id: dict[str, dict[str, Any]] = {}
    for check in checks:
        if not isinstance(check, dict):
            raise VerificationError("every registered check must be an object")
        check_id = check.get("id")
        decider_env = check.get("decider_command_env")
        agent_env = check.get("agent_command_env")
        capabilities = check.get("capabilities")
        timeout = check.get("timeout_seconds")
        if not isinstance(check_id, str) or not check_id:
            raise VerificationError("every registered check needs a non-empty id")
        if check_id in by_id:
            raise VerificationError(f"check registry repeats id {check_id!r}")
        if not isinstance(decider_env, str) or not decider_env:
            raise VerificationError(f"check {check_id!r} needs decider_command_env")
        if not isinstance(agent_env, str) or not agent_env:
            raise VerificationError(f"check {check_id!r} needs agent_command_env")
        if not isinstance(capabilities, list) or not all(isinstance(value, str) for value in capabilities):
            raise VerificationError(f"check {check_id!r} needs a string capabilities array")
        if not isinstance(timeout, int) or timeout <= 0:
            raise VerificationError(f"check {check_id!r} needs a positive timeout_seconds")
        by_id[check_id] = check
    return by_id


def validate_initial_decision(value: dict[str, Any], registry: dict[str, dict[str, Any]]) -> tuple[str, list[CheckRequest]]:
    """Constrain initial-decider output to the checked-in check allowlist."""
    summary = value.get("summary")
    checks = value.get("checks")
    if not isinstance(summary, str) or not summary.strip():
        raise VerificationError("initial decider output needs a non-empty summary")
    if not isinstance(checks, list):
        raise VerificationError("initial decider output needs a checks array")

    selected: list[CheckRequest] = []
    selected_ids: set[str] = set()
    for selection in checks:
        if not isinstance(selection, dict):
            raise VerificationError("each selected check must be an object")
        check_id = selection.get("id")
        task = selection.get("task")
        required = selection.get("required")
        if not isinstance(check_id, str) or check_id not in registry:
            raise VerificationError(f"initial decider selected unknown check {check_id!r}")
        if check_id in selected_ids:
            raise VerificationError(f"initial decider selected {check_id!r} more than once")
        if not isinstance(task, str) or not task.strip():
            raise VerificationError(f"selected check {check_id!r} needs a non-empty task")
        if not isinstance(required, bool):
            raise VerificationError(f"selected check {check_id!r} needs a boolean required value")
        selected_ids.add(check_id)
        selected.append(CheckRequest(id=check_id, task=task, required=required))
    return summary, selected


def validate_check_decision(value: dict[str, Any], check_id: str) -> tuple[str, bool, str | None]:
    """Validate a check-specific decider's choice to run or skip its agent."""
    summary = value.get("summary")
    run_agent = value.get("run_agent")
    agent_task = value.get("agent_task")
    if not isinstance(summary, str) or not summary.strip():
        raise VerificationError(f"check decider {check_id!r} needs a non-empty summary")
    if not isinstance(run_agent, bool):
        raise VerificationError(f"check decider {check_id!r} needs a boolean run_agent")
    if run_agent and (not isinstance(agent_task, str) or not agent_task.strip()):
        raise VerificationError(f"check decider {check_id!r} needs agent_task when run_agent is true")
    if not run_agent and agent_task is not None and not isinstance(agent_task, str):
        raise VerificationError(f"check decider {check_id!r} agent_task must be a string when present")
    return summary, run_agent, agent_task


def validate_agent_result(value: dict[str, Any], check_id: str) -> dict[str, Any]:
    """Validate the stable minimum response contract for a check agent."""
    status = value.get("status")
    summary = value.get("summary")
    findings = value.get("findings", [])
    commands_run = value.get("commands_run", [])
    if status not in VALID_AGENT_STATUSES:
        raise VerificationError(f"agent for check {check_id!r} returned invalid status {status!r}")
    if not isinstance(summary, str) or not summary.strip():
        raise VerificationError(f"agent for check {check_id!r} needs a non-empty summary")
    if not isinstance(findings, list):
        raise VerificationError(f"agent for check {check_id!r} findings must be an array")
    if not isinstance(commands_run, list) or not all(isinstance(item, str) for item in commands_run):
        raise VerificationError(f"agent for check {check_id!r} commands_run must be an array of strings")
    for finding in findings:
        if not isinstance(finding, dict) or not isinstance(finding.get("blocking", False), bool):
            raise VerificationError(f"agent for check {check_id!r} findings need boolean blocking fields")
    return value


def command_result(
    command: str,
    cwd: Path,
    request: dict[str, Any],
    request_path: Path,
    result_path: Path,
    identifier: str,
    write_policy: str,
    timeout_seconds: int,
    stdout_path: Path,
    stderr_path: Path,
) -> tuple[int, dict[str, Any] | None, str]:
    """Run one external decider or agent and collect its protocol result."""
    write_json(request_path, request)
    env = os.environ.copy()
    env.update(
        {
            "VERIFY_CHANGES_REQUEST_FILE": str(request_path),
            "VERIFY_CHANGES_REPORT_FILE": str(result_path),
            "VERIFY_CHANGES_AGENT_ID": identifier,
            "VERIFY_CHANGES_WRITE_POLICY": write_policy,
        }
    )
    try:
        completed = subprocess.run(
            command,
            shell=True,
            cwd=cwd,
            env=env,
            check=False,
            capture_output=True,
            text=True,
            input=json.dumps(request),
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired as error:
        stdout_path.write_text(error.stdout or "")
        stderr_path.write_text(error.stderr or "")
        return 124, None, f"{identifier} timed out after {timeout_seconds} seconds"
    stdout_path.write_text(completed.stdout)
    stderr_path.write_text(completed.stderr)
    output_path = result_path if result_path.exists() else None
    try:
        value = load_json(output_path, f"{identifier} result") if output_path else parse_json_object(completed.stdout, identifier)
    except VerificationError as error:
        return completed.returncode, None, str(error)
    return completed.returncode, value, ""


def status_snapshot(worktree: Path) -> list[str]:
    """Return the concise Git status as stable report data."""
    return run_git(worktree, "status", "--short").splitlines()


def error_result(check: CheckRequest, message: str) -> dict[str, Any]:
    """Create a normalized check failure."""
    return {
        "id": check.id,
        "required": check.required,
        "status": "error",
        "summary": message,
        "findings": [{"blocking": check.required, "title": "Check execution failed", "detail": message}],
    }


def snapshot_tree(worktree: Path) -> str:
    """Stage the isolated worktree and return its complete Git tree object ID."""
    run_git(worktree, "add", "--all")
    return run_git(worktree, "write-tree").strip()


def create_check_worktree(source: Path, head_sha: str, destination: Path, local_patch: str) -> str:
    """Create a detached worker worktree and return its source-state baseline tree."""
    result = subprocess.run(
        ["git", "-C", str(source), "worktree", "add", "--detach", str(destination), head_sha],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        raise VerificationError(result.stderr.strip() or "could not create check worktree")
    try:
        if local_patch:
            run_git(destination, "apply", "--binary", "--whitespace=nowarn", input_text=local_patch)
        return snapshot_tree(destination)
    except VerificationError:
        subprocess.run(["git", "-C", str(source), "worktree", "remove", "--force", str(destination)], check=False)
        raise


def capture_patch(worktree: Path, baseline_tree: str, destination: Path) -> bool:
    """Store edits made after the source-state baseline as an applyable binary patch."""
    after_tree = snapshot_tree(worktree)
    patch = run_git(worktree, "diff", "--binary", baseline_tree, after_tree)
    if not patch:
        return False
    destination.write_text(patch)
    return True


def remove_check_worktree(source: Path, worktree: Path) -> None:
    """Remove a temporary worker worktree and prune its metadata."""
    subprocess.run(["git", "-C", str(source), "worktree", "remove", "--force", str(worktree)], check=False)
    subprocess.run(["git", "-C", str(source), "worktree", "prune"], check=False)


def execute_check(
    selection: CheckRequest,
    registry: dict[str, dict[str, Any]],
    source_worktree: Path,
    context: dict[str, Any],
    artifacts: Path,
    worker_root: Path,
    write_policy: str,
    keep_worktrees: bool,
) -> dict[str, Any]:
    """Run one check decider and, if requested, its agent in an isolated worktree."""
    check = registry[selection.id]
    check_dir = artifacts / "checks" / selection.id
    check_dir.mkdir(parents=True, exist_ok=True)
    worker = worker_root / selection.id
    baseline_tree = ""
    try:
        with WORKTREE_LOCK:
            baseline_tree = create_check_worktree(
                source_worktree, context["head"]["sha"], worker, context["local_patch_from_head"]
            )
    except VerificationError as error:
        return error_result(selection, str(error))

    result: dict[str, Any] = {"id": selection.id, "required": selection.required, "worktree": str(worker)}
    try:
        decider_command = os.environ.get(check["decider_command_env"])
        if not decider_command:
            return error_result(selection, f"missing check decider command: set {check['decider_command_env']}")
        decider_request = {
            "protocol_version": 2,
            "kind": "check-decider-request",
            "check": check,
            "selection": asdict(selection),
            "worktree": str(worker),
            "write_policy": write_policy,
            "change": context,
            "instructions": (
                "Decide whether this specific check needs its worker agent. Set run_agent to false "
                "only when the selected check is not applicable after inspecting the change."
            ),
        }
        exit_code, raw_decision, protocol_error = command_result(
            decider_command,
            worker,
            decider_request,
            check_dir / "decider-request.json",
            check_dir / "decider-result.json",
            f"{selection.id}-decider",
            "deny",
            check["timeout_seconds"],
            check_dir / "decider-stdout.log",
            check_dir / "decider-stderr.log",
        )
        if exit_code or raw_decision is None:
            return error_result(selection, protocol_error or f"check decider exited with {exit_code}")
        summary, run_agent, agent_task = validate_check_decision(raw_decision, selection.id)
        result["decider"] = {"summary": summary, "run_agent": run_agent, "agent_task": agent_task}
        if not run_agent:
            result.update({"status": "skipped", "summary": summary, "findings": [], "patch_available": False})
            return result

        agent_command = os.environ.get(check["agent_command_env"])
        if not agent_command:
            return error_result(selection, f"missing check agent command: set {check['agent_command_env']}")
        before_status = status_snapshot(worker)
        agent_request = {
            "protocol_version": 2,
            "kind": "check-agent-request",
            "check": check,
            "selection": asdict(selection),
            "task": agent_task,
            "worktree": str(worker),
            "write_policy": write_policy,
            "change": context,
            "instructions": (
                "Perform this check thoroughly. You may inspect code and run appropriate tools. "
                "When write_policy is allow, you may make focused fixes or add tests. Report commands "
                "and every blocking finding. Your edits stay in this isolated worktree and are captured "
                "as a patch for deliberate application."
            ),
        }
        exit_code, raw_result, protocol_error = command_result(
            agent_command,
            worker,
            agent_request,
            check_dir / "agent-request.json",
            check_dir / "agent-result.json",
            selection.id,
            write_policy,
            check["timeout_seconds"],
            check_dir / "agent-stdout.log",
            check_dir / "agent-stderr.log",
        )
        after_status = status_snapshot(worker)
        if raw_result is None:
            return error_result(selection, protocol_error)
        agent_result = validate_agent_result(raw_result, selection.id)
        result.update(agent_result)
        result["exit_code"] = exit_code
        result["worktree_status_before"] = before_status
        result["worktree_status_after"] = after_status
        result["worktree_status_changed"] = before_status != after_status
        result["patch_available"] = capture_patch(worker, baseline_tree, check_dir / "agent.patch")
        if exit_code and result["status"] == "pass":
            result["status"] = "error"
            result.setdefault("findings", []).append(
                {"blocking": selection.required, "title": "Agent command failed", "detail": f"exit code {exit_code}"}
            )
        write_json(check_dir / "normalized-result.json", result)
        return result
    except VerificationError as error:
        return error_result(selection, str(error))
    finally:
        if worker.exists() and baseline_tree:
            try:
                capture_patch(worker, baseline_tree, check_dir / "agent.patch")
            except VerificationError:
                pass
        if not keep_worktrees:
            with WORKTREE_LOCK:
                remove_check_worktree(source_worktree, worker)


def is_blocking(result: dict[str, Any]) -> bool:
    """Determine whether a check result prevents successful verification."""
    if result["status"] == "error" and result["required"]:
        return True
    if result["status"] in VALID_AGENT_STATUSES and result["required"] and result["status"] != "pass":
        return True
    return any(finding.get("blocking", False) for finding in result.get("findings", []))


def parse_args() -> argparse.Namespace:
    """Parse CLI options."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worktree", type=Path, default=Path.cwd(), help="Git worktree to review (default: current directory)")
    parser.add_argument("--base", default="HEAD~1", help="base Git ref for the committed diff")
    parser.add_argument("--head", default="HEAD", help="head Git ref for the committed diff")
    parser.add_argument("--registry", type=Path, default=DEFAULT_REGISTRY, help="check registry JSON")
    parser.add_argument("--artifacts-dir", type=Path, default=Path(".verify-changes"), help="directory for reports and logs")
    parser.add_argument("--report-file", type=Path, help="optional path for the aggregate JSON report")
    parser.add_argument("--decider-command", help="override VERIFY_CHANGES_DECIDER_COMMAND")
    parser.add_argument("--max-parallel", type=int, default=4, help="maximum checks to run concurrently")
    parser.add_argument("--keep-worktrees", action="store_true", help="retain isolated check worktrees after execution")
    parser.add_argument("--dry-run", action="store_true", help="invoke and validate only the initial decider")
    parser.add_argument("--read-only", action="store_true", help="tell check agents not to modify their worktrees")
    return parser.parse_args()


def main() -> int:
    """Execute one initial decision and its parallel isolated checks."""
    args = parse_args()
    if args.max_parallel <= 0:
        print("--max-parallel must be positive", file=sys.stderr)
        return 2
    source = args.worktree.resolve()
    artifacts = args.artifacts_dir if args.artifacts_dir.is_absolute() else source / args.artifacts_dir
    artifacts.mkdir(parents=True, exist_ok=True)
    worker_root: Path | None = None
    try:
        registry = load_registry(args.registry.resolve())
        try:
            artifact_prefix = (
                artifacts.resolve().relative_to(source),
            )
        except ValueError:
            artifact_prefix = ()
        context = git_context(source, args.base, args.head, artifact_prefix)
        decider_command = args.decider_command or os.environ.get("VERIFY_CHANGES_DECIDER_COMMAND")
        if not decider_command:
            raise VerificationError("missing initial decider command: set VERIFY_CHANGES_DECIDER_COMMAND or --decider-command")
        write_policy = "deny" if args.read_only else "allow"
        initial_request = {
            "protocol_version": 2,
            "kind": "initial-decider-request",
            "worktree": str(source),
            "write_policy": write_policy,
            "available_checks": list(registry.values()),
            "change": context,
            "instructions": (
                "Select zero or more applicable checks only from available_checks. Each selected check "
                "will independently invoke its own decider and then, if applicable, its own worker agent. "
                "Do not invent check IDs, commands, or capabilities."
            ),
        }
        exit_code, decision, protocol_error = command_result(
            decider_command,
            source,
            initial_request,
            artifacts / "initial-decider-request.json",
            artifacts / "initial-decider-result.json",
            "initial-decider",
            "deny",
            600,
            artifacts / "initial-decider-stdout.log",
            artifacts / "initial-decider-stderr.log",
        )
        if exit_code or decision is None:
            raise VerificationError(protocol_error or f"initial decider command exited with {exit_code}")
        summary, selections = validate_initial_decision(decision, registry)
        report: dict[str, Any] = {
            "protocol_version": 2,
            "generated_at": datetime.now(UTC).isoformat(),
            "source_worktree": str(source),
            "write_policy": write_policy,
            "max_parallel": args.max_parallel,
            "keep_worktrees": args.keep_worktrees,
            "change": context,
            "initial_decider": {"summary": summary, "checks": [asdict(check) for check in selections]},
            "results": [],
        }
        if not args.dry_run and selections:
            worker_root = Path(tempfile.mkdtemp(prefix="verify-changes-"))
            with concurrent.futures.ThreadPoolExecutor(max_workers=min(args.max_parallel, len(selections))) as executor:
                futures = {
                    executor.submit(
                        execute_check,
                        selection,
                        registry,
                        source,
                        context,
                        artifacts,
                        worker_root,
                        write_policy,
                        args.keep_worktrees,
                    ): index
                    for index, selection in enumerate(selections)
                }
                indexed_results = [(index, future.result()) for future, index in futures.items()]
            report["results"] = [result for _, result in sorted(indexed_results)]
        report["passed"] = not any(is_blocking(result) for result in report["results"])
        write_json(artifacts / "report.json", report)
        if args.report_file:
            write_json(args.report_file.resolve(), report)
        print(json.dumps(report, indent=2, sort_keys=True))
        return 0 if report["passed"] else 1
    except VerificationError as error:
        report = {"protocol_version": 2, "passed": False, "error": str(error)}
        write_json(artifacts / "report.json", report)
        if args.report_file:
            write_json(args.report_file.resolve(), report)
        print(json.dumps(report, indent=2, sort_keys=True), file=sys.stderr)
        return 2
    finally:
        if worker_root and not args.keep_worktrees:
            shutil.rmtree(worker_root, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
