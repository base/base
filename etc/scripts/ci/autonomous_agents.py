#!/usr/bin/env python3
"""Plan, run, and publish the repository's autonomous agents.

`.github/agents/registry.json` is the single place that defines each agent: its
label, branch prefix, model, and the minimum and maximum number of pull requests
it may keep open. `.github/workflows/autonomous-agents.yml` calls the subcommands
below; everything that mutates the repository lives here so it can be tested.

The only agent today is `docs-index`, which keeps `etc/docs-index.toml`,
`llms.txt`, and `llms-full.txt` current. The model writes summaries and nothing
else: this script runs `sync`, `stamp`, and `generate`, validates the model's
edits, and publishes a GitHub-signed commit built on top of `main`. Because the
pull request branch is rebuilt from `main` on every refresh, it cannot conflict.
"""

from __future__ import annotations

import argparse
import base64
import importlib.util
import json
import os
import re
import subprocess
import sys
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

ROOT = Path(os.environ.get("AUTONOMOUS_AGENTS_ROOT", Path(__file__).resolve().parents[3]))
REGISTRY_REL = Path(".github/agents/registry.json")
DOCS_INDEX_REL = Path("etc/scripts/local/docs-index.py")
AGENT = "docs-index"
AGENT_FILES = ("etc/docs-index.toml", "llms.txt", "llms-full.txt")
TRIGGERS = ("push", "schedule")
MODEL_RE = re.compile(r"^claude-[a-z0-9][a-z0-9.-]*$")
BRANCH_RE = re.compile(r"^agent/[A-Za-z0-9._/-]+$")
CONTROL_RE = re.compile(r"[\x00-\x1f\x7f]")
LIST_LIMIT = "100"
COMPARE_FILE_LIMIT = 300
CLAUDE_ENV_SCRUB = (
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "ACTIONS_RUNTIME_TOKEN",
    "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    "ACTIONS_ID_TOKEN_REQUEST_URL",
)
CLAUDE_TOOLS = "Read,Edit,Glob,Grep"
CLAUDE_ALLOWED = f"Read(./**),Edit(./{AGENT_FILES[0]}),Glob,Grep"
CLAUDE_DISALLOWED = "Bash,Write,WebFetch,WebSearch,NotebookEdit,Task"


class AgentError(RuntimeError):
    """Raised when the registry, the index, or an agent result is unusable."""


class MainMoved(AgentError):
    """Raised when the base branch advanced during a run, so the result would be stale."""


@dataclass(frozen=True)
class AgentSpec:
    """One agent as defined in the registry."""

    name: str
    label: str
    instructions: str
    branch_prefix: str
    model: str
    trigger: str
    min_open_prs: int
    max_open_prs: int


class GitHub(Protocol):
    """The GitHub operations the controller needs."""

    def json(self, args: list[str]) -> Any: ...

    def run(self, args: list[str]) -> str: ...

    def api(self, method: str, endpoint: str, payload: dict[str, Any] | None = None) -> Any: ...


class GhCli:
    """Run authenticated GitHub CLI commands from the repository root."""

    def run(self, args: list[str]) -> str:
        """Run `gh` and return its stdout."""
        return subprocess.run(
            ["gh", *args], cwd=ROOT, check=True, text=True, stdout=subprocess.PIPE
        ).stdout

    def json(self, args: list[str]) -> Any:
        """Run `gh` and decode its JSON stdout."""
        return json.loads(self.run(args) or "null")

    def api(self, method: str, endpoint: str, payload: dict[str, Any] | None = None) -> Any:
        """Call the GitHub REST API, sending `payload` as the JSON body."""
        command = ["gh", "api", "--method", method, endpoint]
        if payload is not None:
            command.extend(["--input", "-"])
        completed = subprocess.run(
            command,
            cwd=ROOT,
            check=True,
            text=True,
            input=json.dumps(payload) if payload is not None else None,
            stdout=subprocess.PIPE,
        )
        return json.loads(completed.stdout or "null")


def load_docs_index() -> Any:
    """Import `docs-index.py`, whose hyphenated filename rules out a normal import."""
    path = ROOT / DOCS_INDEX_REL
    spec = importlib.util.spec_from_file_location("docs_index", path)
    if spec is None or spec.loader is None:
        raise AgentError(f"cannot load {DOCS_INDEX_REL}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["docs_index"] = module
    spec.loader.exec_module(module)
    return module


def load_registry(path: Path | None = None) -> tuple[dict[str, str], dict[str, AgentSpec]]:
    """Read and validate the registry, returning the target and the agents by name."""
    data = json.loads((path or ROOT / REGISTRY_REL).read_text())
    target = data["target"]
    agents = {}
    for name, raw in data["agents"].items():
        spec = AgentSpec(
            name=name,
            label=str(raw["label"]),
            instructions=str(raw["instructions"]),
            branch_prefix=str(raw["branch_prefix"]),
            model=str(raw["model"]),
            trigger=str(raw["trigger"]),
            min_open_prs=raw["min_open_prs"],
            max_open_prs=raw["max_open_prs"],
        )
        errors = []
        if not MODEL_RE.match(spec.model):
            errors.append("model must look like `claude-...`")
        if not BRANCH_RE.match(spec.branch_prefix) or not spec.branch_prefix.endswith("-"):
            errors.append("branch_prefix must start with `agent/` and end with `-`")
        if spec.trigger not in TRIGGERS:
            errors.append(f"trigger must be one of {', '.join(TRIGGERS)}")
        if not spec.label:
            errors.append("label is required")
        if not all(isinstance(n, int) and not isinstance(n, bool) for n in (spec.min_open_prs, spec.max_open_prs)):
            errors.append("min_open_prs and max_open_prs must be integers")
        elif not 0 <= spec.min_open_prs <= spec.max_open_prs or spec.max_open_prs < 1:
            errors.append("require 0 <= min_open_prs <= max_open_prs and max_open_prs >= 1")
        if errors:
            raise AgentError(f"registry agent {name!r}: {'; '.join(errors)}")
        agents[name] = spec
    return {"repository": str(target["repository"]), "base_branch": str(target["base_branch"])}, agents


def open_agent_prs(github: GitHub, target: dict[str, str], spec: AgentSpec) -> list[dict[str, Any]]:
    """Return the agent's open pull requests, oldest first."""
    pulls = github.json(
        [
            "pr", "list", "--repo", target["repository"], "--base", target["base_branch"],
            "--state", "open", "--label", spec.label, "--limit", LIST_LIMIT,
            "--json", "number,headRefName",
        ]
    )  # fmt: skip
    found = [
        {"pull_request": int(pull["number"]), "branch": str(pull["headRefName"])}
        for pull in pulls
        if str(pull.get("headRefName", "")).startswith(spec.branch_prefix)
    ]
    return sorted(found, key=lambda pull: pull["pull_request"])


def plan_agent(spec: AgentSpec, open_prs: list[dict[str, Any]], needed: bool) -> dict[str, Any]:
    """Decide which runs to start and which pull requests to close.

    With no work, every open pull request is closed, because the index is
    already current. With work, the oldest open pull requests up to
    `max_open_prs` are refreshed and the rest are closed. A run is also started
    to create a pull request when fewer than `min_open_prs` are open, or when
    none is open at all. That run is a candidate: `command_publish` lets the
    agent decline it when its result changes nothing and `min_open_prs` allows.
    """
    if not needed:
        return {"include": [], "close": open_prs}
    kept, excess = open_prs[: spec.max_open_prs], open_prs[spec.max_open_prs :]
    include = [{"mode": "refresh", **pull} for pull in kept]
    creates = max(spec.min_open_prs, 1) - len(kept)
    include += [{"mode": "create", "pull_request": 0, "branch": ""} for _ in range(max(0, creates))]
    return {"include": include, "close": excess}


def docs_index_needed(index: Any) -> bool:
    """Return whether the committed docs index differs from the docs on disk."""
    entries = index.load_manifest(ROOT)
    return bool(index.find_problems(entries, index.list_docs(ROOT)) or index.stale_outputs(ROOT, entries))


def write_github_outputs(path: Path, outputs: dict[str, Any]) -> None:
    """Append step outputs in the multiline-safe format; non-strings are JSON encoded."""
    with path.open("a") as output:
        for key, value in outputs.items():
            encoded = value if isinstance(value, str) else json.dumps(value, separators=(",", ":"))
            delimiter = f"ghadelim_{uuid.uuid4().hex}"
            output.write(f"{key}<<{delimiter}\n{encoded}\n{delimiter}\n")


def event_matches_trigger(spec: AgentSpec, event_name: str) -> bool:
    """Return whether an event may run the agent.

    The workflow listens for every trigger because `on:` cannot read the
    registry, so the registry's `trigger` decides which events do work. Manual
    dispatch always runs, so an operator can start any agent.
    """
    return event_name not in TRIGGERS or event_name == spec.trigger


def command_plan(github: GitHub, agent: str, event_name: str, github_output: Path | None) -> int:
    """Print the reconciliation plan and expose it as step outputs."""
    target, agents = load_registry()
    spec = agents[agent]
    if not event_matches_trigger(spec, event_name):
        print(f"{spec.name} runs on {spec.trigger}, not {event_name}; nothing to do")
        plan: dict[str, Any] = {"include": [], "close": []}
        needed, open_count = False, 0
    else:
        needed = docs_index_needed(load_docs_index())
        open_prs = open_agent_prs(github, target, spec)
        plan, open_count = plan_agent(spec, open_prs, needed), len(open_prs)
    outputs = {
        "needed": "true" if needed else "false",
        "open_count": open_count,
        "matrix": {"include": plan["include"]},
        "count": len(plan["include"]),
        "close": plan["close"],
        "close_count": len(plan["close"]),
    }
    if github_output:
        write_github_outputs(github_output, outputs)
    print(json.dumps(outputs, indent=2))
    return 0


def command_close(github: GitHub, agent: str, closing: list[dict[str, Any]]) -> int:
    """Close pull requests the plan no longer wants and delete their branches."""
    target, agents = load_registry()
    spec = agents[agent]
    live = {pull["pull_request"]: pull["branch"] for pull in open_agent_prs(github, target, spec)}
    for pull in closing:
        number = int(pull["pull_request"])
        if live.get(number) != pull["branch"]:
            print(f"skipping #{number}: no longer an open {spec.name} pull request")
            continue
        github.run(
            [
                "pr", "close", str(number), "--repo", target["repository"], "--delete-branch",
                "--comment", "Closing: the docs index on main is already current, or this agent "
                "already has its maximum number of open pull requests.",
            ]
        )  # fmt: skip
        print(f"closed #{number}")
    return 0


@dataclass(frozen=True)
class Work:
    """The docs whose summaries the model must write or re-review."""

    new: list[str]
    changed: list[str]

    @property
    def listed(self) -> list[str]:
        """Every doc the model was asked about."""
        return self.new + self.changed

    def to_json(self) -> str:
        """Serialize for the runner-temporary work file."""
        return json.dumps({"new": self.new, "changed": self.changed})

    @classmethod
    def from_json(cls, text: str) -> Work:
        """Parse the runner-temporary work file."""
        data = json.loads(text)
        return cls(new=list(data["new"]), changed=list(data["changed"]))


def classify_work(index: Any, entries: list[Any], docs: dict[str, str]) -> Work:
    """Split the entries that need a human-quality summary into new and changed docs."""
    new, changed = [], []
    for entry in entries:
        if entry.summary.startswith(index.PLACEHOLDER):
            new.append(entry.path)
        elif len(entry.summary) > index.MAX_SUMMARY_LEN or entry.digest != docs[entry.path]:
            changed.append(entry.path)
    return Work(new, changed)


def prepare_entries(
    index: Any, entries: list[Any], docs: dict[str, str], previous: list[Any] | None
) -> tuple[list[Any], Work]:
    """Sync the manifest with the docs, reusing summaries an earlier run already wrote.

    A previous summary is reused only when it was reviewed against exactly the
    doc's current content, so a refresh asks the model about the delta alone.
    """
    synced = index.sync_entries(entries, docs)
    reusable = {
        entry.path: entry
        for entry in previous or []
        if entry.path in docs
        and entry.digest == docs[entry.path]
        and not entry.summary.startswith(index.PLACEHOLDER)
        and len(entry.summary) <= index.MAX_SUMMARY_LEN
    }
    merged = []
    for entry in synced:
        stale = entry.summary.startswith(index.PLACEHOLDER) or entry.digest != docs[entry.path]
        merged.append(reusable[entry.path] if stale and entry.path in reusable else entry)
    return merged, classify_work(index, merged, docs)


def command_prepare(previous: Path | None, work_path: Path, baseline_path: Path) -> int:
    """Sync the manifest and record what the model must do."""
    index = load_docs_index()
    docs = index.list_docs(ROOT)
    previous_entries = index.parse_manifest(previous.read_text()) if previous else None
    entries, work = prepare_entries(index, index.load_manifest(ROOT), docs, previous_entries)
    index.write_manifest(ROOT, entries)
    baseline_path.write_text((ROOT / index.MANIFEST_REL).read_text())
    work_path.write_text(work.to_json())
    print(json.dumps({"new": work.new, "changed": work.changed}, indent=2))
    return 0


def validate_summaries(index: Any, work: Work, baseline_text: str, current_text: str) -> list[str]:
    """Check the model's manifest against the prepared baseline, returning each violation."""
    try:
        baseline = index.parse_manifest(baseline_text)
        current = index.parse_manifest(current_text)
    except index.DocsIndexError as error:
        return [f"{index.MANIFEST_REL} is not a valid manifest: {error}"]
    if [entry.path for entry in baseline] != [entry.path for entry in current]:
        return ["do not add, remove, rename, or reorder manifest entries"]
    errors = []
    allowed, new = set(work.listed), set(work.new)
    for before, after in zip(baseline, current, strict=True):
        if after.digest != before.digest:
            errors.append(f"{after.path}: do not edit `digest`; the workflow stamps it")
        if after.summary == before.summary:
            if after.path in new:
                errors.append(f"{after.path}: replace the TODO summary")
            elif after.path in allowed and len(after.summary) > index.MAX_SUMMARY_LEN:
                errors.append(f"{after.path}: shorten the summary to {index.MAX_SUMMARY_LEN} characters")
            continue
        if after.path not in allowed:
            errors.append(f"{after.path}: not a listed doc; restore its summary")
        elif not after.summary.strip() or CONTROL_RE.search(after.summary):
            errors.append(f"{after.path}: summary must be one non-empty line")
        elif after.summary.startswith(index.PLACEHOLDER):
            errors.append(f"{after.path}: replace the TODO summary")
        elif len(after.summary) > index.MAX_SUMMARY_LEN:
            errors.append(f"{after.path}: summary is {len(after.summary)} characters (max {index.MAX_SUMMARY_LEN})")
    return errors


def git_lines(*args: str) -> list[str]:
    """Run git in the repository and return its non-empty output lines."""
    out = subprocess.run(["git", *args], cwd=ROOT, check=True, text=True, stdout=subprocess.PIPE).stdout
    return [line for line in out.splitlines() if line]


def changed_files() -> list[str]:
    """Return every path the working tree changes relative to HEAD, including untracked files."""
    tracked = git_lines("diff", "--name-only", "HEAD")
    untracked = git_lines("ls-files", "--others", "--exclude-standard")
    return sorted(set(tracked + untracked))


def validation_errors(work_path: Path, baseline_path: Path) -> list[str]:
    """Validate the working tree after a model pass."""
    index = load_docs_index()
    work = Work.from_json(work_path.read_text())
    errors = validate_summaries(
        index, work, baseline_path.read_text(), (ROOT / index.MANIFEST_REL).read_text()
    )
    outside = [path for path in changed_files() if path != str(index.MANIFEST_REL)]
    if outside:
        errors.append(f"only {index.MANIFEST_REL} may change; restore: {', '.join(outside)}")
    return errors


def render_claude_event(event: dict[str, Any]) -> list[str]:
    """Render one stream-json event as sanitized trace lines, without tool output."""
    kind = event.get("type")
    if kind == "system" and event.get("subtype") == "init":
        return [f"[claude] session initialized ({event.get('model') or 'configured model'})"]
    rendered = []
    if kind == "assistant":
        for content in event.get("message", {}).get("content", []):
            if content.get("type") == "text" and content.get("text"):
                rendered.extend(f"[assistant] {line}" for line in str(content["text"]).splitlines())
            elif content.get("type") == "tool_use":
                tool_input = content.get("input", {})
                target = next((str(tool_input[k]) for k in ("file_path", "path", "pattern") if tool_input.get(k)), "")
                rendered.append(f"[tool] {content.get('name', 'unknown')} {target}".rstrip())
    elif kind == "user":
        for content in event.get("message", {}).get("content", []):
            if content.get("type") == "tool_result" and content.get("is_error"):
                rendered.append("[tool-result] error")
    elif kind == "result":
        rendered.append(f"[claude] result subtype={event.get('subtype', 'unknown')}")
    return rendered


def run_claude(claude: Path, model: str, prompt: str, stream_path: Path) -> None:
    """Run Claude with edit access to the manifest only, streaming a sanitized trace.

    Workflow-command processing is suspended while model output is printed so
    generated text cannot inject log commands.
    """
    environment = {k: v for k, v in os.environ.items() if k not in CLAUDE_ENV_SCRUB}
    command = [
        str(claude), "-p", prompt, "--model", model, "--permission-mode", "acceptEdits",
        "--no-session-persistence", "--output-format", "stream-json", "--verbose",
        "--tools", CLAUDE_TOOLS, "--allowedTools", CLAUDE_ALLOWED,
        "--disallowedTools", CLAUDE_DISALLOWED,
    ]  # fmt: skip
    stop_token = f"agent-{uuid.uuid4()}"
    print(f"::stop-commands::{stop_token}", flush=True)
    try:
        with (
            stream_path.open("w") as stream,
            subprocess.Popen(command, cwd=ROOT, env=environment, text=True, stdout=subprocess.PIPE) as process,
        ):
            # Popen's exit waits for the child, so kill it first on any error.
            try:
                assert process.stdout
                for line in process.stdout:
                    stream.write(line)
                    if not line.strip():
                        continue
                    try:
                        event = json.loads(line)
                    except json.JSONDecodeError:
                        print("[claude] non-JSON output suppressed", flush=True)
                        continue
                    for rendered in render_claude_event(event):
                        print(rendered, flush=True)
                return_code = process.wait()
            except BaseException:
                process.kill()
                raise
    finally:
        print(f"::{stop_token}::", flush=True)
    if return_code:
        raise subprocess.CalledProcessError(return_code, command)


def build_prompt(spec: AgentSpec, work: Work, feedback: str) -> str:
    """Compose the model prompt for one pass."""
    lines = [
        f"Read `{spec.instructions}` and `.agents/skills/update-docs-index/SKILL.md`, then follow them exactly.",
        "",
        "New docs (replace the TODO summary in etc/docs-index.toml):",
        *[f"- {path}" for path in work.new],
        "Changed docs (edit the summary only if the doc no longer supports it):",
        *[f"- {path}" for path in work.changed],
        "",
        "Edit only etc/docs-index.toml. Do not touch any digest, and do not run commands.",
    ]
    if feedback:
        lines += [
            "",
            "This is a repair pass. Fix every finding below without discarding valid summaries.",
            feedback[-4000:],
        ]
    return "\n".join(lines)


def command_run_agent(
    agent: str, claude: Path, work_path: Path, baseline_path: Path, attempts: int
) -> int:
    """Run the model until its edits validate or the attempts run out."""
    _, agents = load_registry()
    spec = agents[agent]
    work = Work.from_json(work_path.read_text())
    if not work.listed:
        print("no summaries to write; finalize will only regenerate the index files")
        return 0
    runner_temp = Path(os.environ.get("RUNNER_TEMP", "/tmp"))
    feedback = ""
    for attempt in range(1, attempts + 1):
        run_claude(claude, spec.model, build_prompt(spec, work, feedback), runner_temp / f"{agent}-{attempt}.jsonl")
        errors = validation_errors(work_path, baseline_path)
        if not errors:
            return 0
        feedback = "\n".join(f"ERROR: {error}" for error in errors)
        print(f"attempt {attempt} failed validation:\n{feedback}", flush=True)
    print(feedback, file=sys.stderr)
    return 1


def is_substantive(index: Any, main_entries: list[Any], final_entries: list[Any]) -> bool:
    """Return whether the result differs from `main` by more than digests.

    An added or removed entry, or a changed summary, is a real update. Restamping
    a digest after the agent confirmed a summary is still accurate is not: it
    records a review and tells readers nothing new, so it never justifies a pull
    request by itself.
    """
    before = {entry.path: entry.summary for entry in main_entries}
    after = {entry.path: entry.summary for entry in final_entries}
    return before != after


def command_finalize(work_path: Path, baseline_path: Path, report_path: Path) -> int:
    """Stamp the reviewed docs, regenerate the index files, and verify the result."""
    index = load_docs_index()
    work = Work.from_json(work_path.read_text())
    errors = validation_errors(work_path, baseline_path)
    if errors:
        print("\n".join(f"ERROR: {error}" for error in errors), file=sys.stderr)
        return 1
    docs = index.list_docs(ROOT)
    entries = index.stamp_entries(index.load_manifest(ROOT), docs, work.listed)
    index.write_manifest(ROOT, entries)
    index.write_outputs(ROOT, entries)
    problems = index.find_problems(entries, docs) + index.stale_outputs(ROOT, entries)
    if problems:
        print("\n".join(f"ERROR: {problem}" for problem in problems), file=sys.stderr)
        return 1
    baseline = {entry.path: entry.summary for entry in index.parse_manifest(baseline_path.read_text())}
    summaries = {entry.path: entry.summary for entry in entries}
    main_text = subprocess.run(
        ["git", "show", f"HEAD:{index.MANIFEST_REL}"], cwd=ROOT, check=True, text=True, stdout=subprocess.PIPE
    ).stdout
    report = {
        "substantive": is_substantive(index, index.parse_manifest(main_text), entries),
        "added": work.new,
        "updated": [p for p in work.changed if summaries[p] != baseline[p]],
        "confirmed": [p for p in work.changed if summaries[p] == baseline[p]],
        "files": changed_files(),
    }
    report_path.write_text(json.dumps(report))
    print(json.dumps(report, indent=2))
    return 0


def render_pr_body(report: dict[str, Any], spec: AgentSpec) -> str:
    """Render the deterministic pull-request description."""

    def section(title: str, paths: list[str]) -> list[str]:
        return [f"### {title}", *[f"- `{path}`" for path in paths], ""] if paths else []

    lines = [
        "Brings the docs index (`etc/docs-index.toml`, `llms.txt`, `llms-full.txt`) up to date with `main`.",
        "",
        *section("New docs summarized", report["added"]),
        *section("Summaries rewritten", report["updated"]),
        *section("Changed docs re-read, summary still accurate", report["confirmed"]),
        "### Maintenance",
        "",
        f"The `{spec.name}` autonomous agent (`.github/agents/registry.json`) owns this branch. "
        "It rebuilds the branch from `main` whenever docs change, so it never conflicts; do not push to it. "
        "It closes this pull request when the index is already current. Feature pull requests should not "
        "edit the index.",
    ]
    return "\n".join(lines) + "\n"


def branch_is_current(github: GitHub, repository: str, base_branch: str, branch: str, local: dict[str, bytes]) -> bool:
    """Return whether the pull request branch already holds `local` and cannot conflict with `base_branch`."""
    tip = github.api("GET", f"repos/{repository}/git/ref/heads/{branch}")["object"]["sha"]
    parent = github.api("GET", f"repos/{repository}/git/commits/{tip}")["parents"][0]["sha"]
    for path, data in local.items():
        remote = github.api("GET", f"repos/{repository}/contents/{path}?ref={branch}")
        if base64.b64decode(remote["content"]) != data:
            return False
    comparison = github.api("GET", f"repos/{repository}/compare/{parent}...{base_branch}")
    files = comparison.get("files", [])
    if len(files) >= COMPARE_FILE_LIMIT:
        return False
    return not {entry["filename"] for entry in files} & set(AGENT_FILES)


def commit_files(
    github: GitHub, repository: str, base_branch: str, branch: str, message: str,
    local: dict[str, bytes], update_existing: bool,
) -> str:  # fmt: skip
    """Publish `local` as one commit on top of `base_branch` through the Git Database API.

    Commits created this way with the workflow token are signed by GitHub.
    """
    base_sha = str(github.api("GET", f"repos/{repository}/git/ref/heads/{base_branch}")["object"]["sha"])
    if git_lines("rev-parse", "HEAD")[0] != base_sha:
        raise MainMoved(f"{base_branch} moved during the run; a later run will retry")
    base_tree = str(github.api("GET", f"repos/{repository}/git/commits/{base_sha}")["tree"]["sha"])
    entries = []
    for path, data in local.items():
        blob = github.api(
            "POST", f"repos/{repository}/git/blobs",
            {"content": base64.b64encode(data).decode(), "encoding": "base64"},
        )  # fmt: skip
        entries.append({"path": path, "mode": "100644", "type": "blob", "sha": blob["sha"]})
    tree = github.api("POST", f"repos/{repository}/git/trees", {"base_tree": base_tree, "tree": entries})
    commit = github.api(
        "POST", f"repos/{repository}/git/commits",
        {"message": message, "tree": tree["sha"], "parents": [base_sha]},
    )  # fmt: skip
    if update_existing:
        github.api("PATCH", f"repos/{repository}/git/refs/heads/{branch}", {"sha": commit["sha"], "force": True})
    else:
        github.api("POST", f"repos/{repository}/git/refs", {"ref": f"refs/heads/{branch}", "sha": commit["sha"]})
    return str(commit["sha"])


def command_publish(
    github: GitHub, agent: str, mode: str, branch: str, pull_request: int, report_path: Path
) -> int:
    """Publish the finalized index as a signed commit and open or update the pull request."""
    target, agents = load_registry()
    spec, repository, base_branch = agents[agent], target["repository"], target["base_branch"]
    if mode == "create":
        branch = f"{spec.branch_prefix}{os.environ['GITHUB_RUN_ID']}-{os.environ['GITHUB_RUN_ATTEMPT']}"
    if not BRANCH_RE.match(branch) or not branch.startswith(spec.branch_prefix):
        raise AgentError(f"refusing to publish to {branch!r}")
    report = json.loads(report_path.read_text())
    local = {path: (ROOT / path).read_bytes() for path in AGENT_FILES}
    message = "docs: refresh docs index"
    open_count = len(open_agent_prs(github, target, spec))

    if not report["substantive"]:
        # The agent decided no update is needed, so leave the repository alone
        # unless that would drop the open pull requests below the registry floor.
        if mode == "create" and open_count >= spec.min_open_prs:
            print(f"{spec.name} decided no update is needed; nothing to open")
            return 0
        if mode == "refresh" and open_count - 1 >= spec.min_open_prs:
            github.run(
                ["pr", "close", str(pull_request), "--repo", repository, "--delete-branch",
                 "--comment", "Closing: on the latest main the agent decided no index update is needed."]
            )  # fmt: skip
            print(f"closed #{pull_request}: no update is needed")
            return 0

    if mode == "create":
        if open_count >= spec.max_open_prs:
            print(f"{spec.name} already has {spec.max_open_prs} open pull request(s); nothing to open")
            return 0
    elif branch_is_current(github, repository, base_branch, branch, local):
        print(f"#{pull_request} already holds the current index; nothing to publish")
        return 0

    previous = None
    if mode == "refresh":
        previous = str(github.api("GET", f"repos/{repository}/git/ref/heads/{branch}")["object"]["sha"])
    sha = commit_files(github, repository, base_branch, branch, message, local, mode == "refresh")
    verified = github.api("GET", f"repos/{repository}/commits/{sha}")["commit"]["verification"]["verified"]
    if not verified:
        if previous:
            github.api("PATCH", f"repos/{repository}/git/refs/heads/{branch}", {"sha": previous, "force": True})
        else:
            github.api("DELETE", f"repos/{repository}/git/refs/heads/{branch}")
        raise AgentError("published commit was not GitHub-verified; rolled back")

    body = render_pr_body(report, spec)
    body_path = Path(os.environ.get("RUNNER_TEMP", "/tmp")) / f"{spec.name}-pr.md"
    body_path.write_text(body)
    if mode == "create":
        github.run(["label", "create", spec.label, "--repo", repository, "--force", "--color", "0e8a16",
                    "--description", f"Opened by the {spec.name} autonomous agent"])  # fmt: skip
        url = github.run(
            ["pr", "create", "--repo", repository, "--base", base_branch, "--head", branch,
             "--label", spec.label, "--title", message, "--body-file", str(body_path)]
        ).strip()  # fmt: skip
        print(f"opened {url}")
    else:
        github.run(["pr", "edit", str(pull_request), "--repo", repository, "--body-file", str(body_path)])
        print(f"refreshed #{pull_request}")
    return 0


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """Parse CLI arguments."""
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    def add(name: str, *, agent: bool = True) -> argparse.ArgumentParser:
        command = sub.add_parser(name)
        if agent:
            command.add_argument("--agent", default=AGENT)
        return command

    plan = add("plan")
    plan.add_argument("--event-name", required=True, help="GitHub event that started the run")
    plan.add_argument("--github-output", type=Path)
    close = add("close")
    close.add_argument("--pull-requests", required=True, help="JSON list from the plan's `close` output")
    prepare = add("prepare", agent=False)
    prepare.add_argument("--previous", type=Path, help="manifest from the open pull request branch")
    prepare.add_argument("--work", type=Path, required=True)
    prepare.add_argument("--baseline", type=Path, required=True)
    run = add("run-agent")
    run.add_argument("--claude", type=Path, required=True)
    run.add_argument("--work", type=Path, required=True)
    run.add_argument("--baseline", type=Path, required=True)
    run.add_argument("--attempts", type=int, default=3)
    finalize = add("finalize", agent=False)
    finalize.add_argument("--work", type=Path, required=True)
    finalize.add_argument("--baseline", type=Path, required=True)
    finalize.add_argument("--report", type=Path, required=True)
    publish = add("publish")
    publish.add_argument("--mode", choices=("create", "refresh"), required=True)
    publish.add_argument("--branch", default="")
    publish.add_argument("--pull-request", type=int, default=0)
    publish.add_argument("--report", type=Path, required=True)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    """CLI entrypoint."""
    args = parse_args(argv)
    try:
        if args.command == "plan":
            return command_plan(GhCli(), args.agent, args.event_name, args.github_output)
        if args.command == "close":
            return command_close(GhCli(), args.agent, json.loads(args.pull_requests))
        if args.command == "prepare":
            return command_prepare(args.previous, args.work, args.baseline)
        if args.command == "run-agent":
            return command_run_agent(args.agent, args.claude, args.work, args.baseline, args.attempts)
        if args.command == "finalize":
            return command_finalize(args.work, args.baseline, args.report)
        return command_publish(GhCli(), args.agent, args.mode, args.branch, args.pull_request, args.report)
    except MainMoved as error:
        # Not a failure: the next run of the agent's trigger starts from the new main.
        print(f"notice: {error}")
        return 0
    except (AgentError, OSError, KeyError, IndexError, TypeError, ValueError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
