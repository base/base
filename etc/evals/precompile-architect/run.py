#!/usr/bin/env python3
"""Run precompile-architect eval trials (runner stage of the eval loop).

Every trial runs in a fresh sandbox:

- Repo: an empty repository, then `git fetch <local repo> <base_commit>` with no depth limit and a
  detached checkout. Ancestors stay available for git log and blame. Descendants, including the
  reference commit, are never fetched. The run refuses any base commit that already contains
  the eval directory, so cases and answers cannot appear in the checkout.
- Config: a throwaway CLAUDE_CONFIG_DIR per trial, so no user memory, user skills or settings.
  The treatment arm installs only the skill under test into that directory.
- Tools: Read, Grep, Glob, Bash and Skill. --strict-mcp-config with an empty MCP config. Bash runs
  in Claude Code's sandbox with an empty network allowlist and no unsandboxed escape hatch, so gh,
  git fetch and curl cannot reach anything. Only the claude process reaches the model endpoint.

Arms: `treatment` has the skill, `control` does not. Both get the same harness prompt, which
includes the plan schema verbatim, so the skill is never the only source of the output format.

Output layout: results/<run_id>/<case_id>/<arm>/trial-<n>.json, with the raw transcript beside it
as trial-<n>.transcript.json.

Usage:
  python3 run.py --run-id dev-01 --split dev --model <architect> [--skill <dir>] [--arms control treatment]
  python3 run.py --run-id m1-holdout --split holdout --milestone "skill v1" --model <architect> --skill <dir>
"""

import argparse
import concurrent.futures
import datetime
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from evallib import PROMPTS, REPO, ROOT, case_set_sha, exists_at, extract_plan, load_cases, schema  # noqa: E402
from grade import grade_plan  # noqa: E402
from judge import DEFAULT_JUDGE_MODEL, fork_table, judge_case  # noqa: E402

RESULTS = ROOT / "results"
HOLDOUT_LOG = ROOT / "holdout_log.jsonl"
EVAL_DIR = str(ROOT.relative_to(REPO))
TRIALS_BY_SPLIT = {"dev": 3, "holdout": 5}
ALLOWED_TOOLS = "Read Grep Glob Bash Skill"
DISALLOWED_TOOLS = "Edit Write NotebookEdit WebFetch WebSearch Agent"
SANDBOX_SETTINGS = {
    "sandbox": {
        "enabled": True,
        "allowUnsandboxedCommands": False,
        "autoAllowBashIfSandboxed": True,
        "network": {"allowedDomains": []},
    }
}
EMPTY_MCP = json.dumps({"mcpServers": {}})
# Variables from a parent Claude Code session that would leak into the child.
PARENT_SESSION_VARS = (
    "CLAUDECODE", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SESSION_ATTENDED", "CLAUDE_PID",
)


def build_prompt(case):
    return (PROMPTS / "task.md").read_text().format(
        ticket=case["ticket"],
        fork_table=fork_table(case["fork_state"]),
        output_contract=(PROMPTS / "output_contract.md").read_text(),
        plan_schema=json.dumps(schema("plan.schema.json"), indent=2),
    )


def checkout(commit, dest):
    """Fresh repo at `commit` with its full ancestry and nothing after it."""
    for command in (["init", "-q"], ["fetch", "-q", "--no-tags", str(REPO), commit], ["checkout", "-q", "--detach", commit]):
        subprocess.run(["git", *command], cwd=dest, check=True, capture_output=True)


def tree_sha(path):
    digest = hashlib.sha256()
    for file in sorted(p for p in Path(path).rglob("*") if p.is_file()):
        digest.update(str(file.relative_to(path)).encode())
        digest.update(file.read_bytes())
    return digest.hexdigest()[:16]


def claude_version():
    return subprocess.run(["claude", "--version"], capture_output=True, text=True).stdout.strip()


def run_trial(case, arm, trial, args, meta):
    out_dir = RESULTS / args.run_id / case["id"] / arm
    out_dir.mkdir(parents=True, exist_ok=True)
    record_path = out_dir / f"trial-{trial}.json"
    transcript_path = out_dir / f"trial-{trial}.transcript.json"
    prompt = build_prompt(case)

    with tempfile.TemporaryDirectory(prefix="pa-eval-") as tmp:
        workspace, config = Path(tmp) / "repo", Path(tmp) / "config"
        workspace.mkdir()
        config.mkdir()
        checkout(case["base_commit"], workspace)
        if arm == "treatment":
            shutil.copytree(args.skill, config / "skills" / Path(args.skill).name)
        settings = Path(tmp) / "settings.json"
        settings.write_text(json.dumps(SANDBOX_SETTINGS))
        env = {k: v for k, v in os.environ.items() if k not in PARENT_SESSION_VARS}
        env["CLAUDE_CONFIG_DIR"] = str(config)
        command = [
            "claude", "-p", prompt, "--model", args.model, "--output-format", "json", "--no-session-persistence",
            "--strict-mcp-config", "--mcp-config", EMPTY_MCP, "--settings", str(settings),
            "--permission-mode", "dontAsk", "--allowedTools", ALLOWED_TOOLS, "--disallowedTools", DISALLOWED_TOOLS,
            "--max-budget-usd", str(args.max_budget_usd),
        ]
        started = time.time()
        try:
            result = subprocess.run(command, cwd=workspace, env=env, capture_output=True, text=True,
                                    timeout=args.timeout, stdin=subprocess.DEVNULL)
            stdout, stderr, timed_out = result.stdout, result.stderr, False
        except subprocess.TimeoutExpired as exc:
            stdout, stderr, timed_out = exc.stdout or "", exc.stderr or "", True
        elapsed = round(time.time() - started, 1)

    try:
        payload = json.loads(stdout)
    except ValueError:
        payload = {}
    final_text = payload.get("result", "") if isinstance(payload, dict) else ""
    transcript_path.write_text(json.dumps(
        {"prompt": prompt, "final_text": final_text, "payload": payload, "stderr": stderr[-4000:], "timed_out": timed_out},
        indent=2))

    plan, repairs, parse_error = extract_plan(final_text)
    infra_error = timed_out or not payload or bool(payload.get("is_error"))
    judge = judge_case(case, final_text, args.judge_model) if plan is not None and not args.no_judge else None
    if plan is not None:
        grade = grade_plan(case, plan, judge)
    else:
        grade = {"pass": False, "code_pass": False, "hard_fails": ["format_failure"], "parse_error": parse_error}

    record = {
        "case_id": case["id"],
        "arm": arm,
        "trial": trial,
        "split": case["split"],
        "architect_model": sorted((payload.get("modelUsage") or {}).keys()) or [args.model],
        "judge_model": judge["judge_model"] if judge else None,
        "claude_code_version": meta["claude_code_version"],
        "skill_sha": meta["skill_sha"] if arm == "treatment" else None,
        "case_set_sha": meta["case_set_sha"],
        "timestamp": datetime.datetime.now(datetime.UTC).isoformat(timespec="seconds"),
        "transcript_path": str(transcript_path.relative_to(ROOT)),
        "infra_error": bool(infra_error and plan is None),
        "format_repairs": repairs,
        "plan": plan,
        "grade": grade,
        "judge": judge,
        "cost_usd": payload.get("total_cost_usd") if isinstance(payload, dict) else None,
        "num_turns": payload.get("num_turns") if isinstance(payload, dict) else None,
        "elapsed_s": elapsed,
    }
    record_path.write_text(json.dumps(record, indent=2))
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--split", choices=["dev", "holdout"], required=True)
    parser.add_argument("--arms", nargs="+", choices=["control", "treatment"], default=["control", "treatment"])
    parser.add_argument("--skill", help="skill directory containing SKILL.md, required for the treatment arm")
    parser.add_argument("--model", required=True, help="architect model id")
    parser.add_argument("--judge-model", default=DEFAULT_JUDGE_MODEL)
    parser.add_argument("--no-judge", action="store_true", help="skip the judge; no trial can pass without it")
    parser.add_argument("--trials", type=int, help="override k (defaults: dev 3, holdout 5)")
    parser.add_argument("--cases", nargs="*", help="restrict to these case ids within the split")
    parser.add_argument("--milestone", help="required for holdout runs; logged to holdout_log.jsonl")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--timeout", type=int, default=1200, help="seconds per trial")
    parser.add_argument("--max-budget-usd", type=float, default=5.0, help="per trial")
    args = parser.parse_args()

    if "treatment" in args.arms and not (args.skill and (Path(args.skill) / "SKILL.md").is_file()):
        parser.error("the treatment arm needs --skill pointing at a directory with SKILL.md")
    if args.split == "holdout" and not args.milestone:
        parser.error("holdout runs only at declared milestones; pass --milestone")
    if (RESULTS / args.run_id).exists():
        parser.error(f"results/{args.run_id} already exists; pick a new run id")

    cases = [c for c in load_cases(args.cases, args.split) if c["split"] == args.split]
    for case in cases:
        if exists_at(case["base_commit"], EVAL_DIR):
            raise SystemExit(f"{case['id']}: base_commit contains {EVAL_DIR}; re-pin it before the eval directory was committed")
    k = args.trials or TRIALS_BY_SPLIT[args.split]
    meta = {
        "claude_code_version": claude_version(),
        "skill_sha": tree_sha(args.skill) if args.skill else None,
        "case_set_sha": case_set_sha(),
    }
    run_dir = RESULTS / args.run_id
    run_dir.mkdir(parents=True)
    config = {k_: v for k_, v in vars(args).items()} | meta | {"k": k, "case_ids": [c["id"] for c in cases]}
    (run_dir / "run.json").write_text(json.dumps(config, indent=2))

    jobs = [(case, arm, trial) for case in cases for arm in args.arms for trial in range(1, k + 1)]
    print(f"{len(jobs)} sessions: {len(cases)} cases x {len(args.arms)} arms x k={k}")
    records = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = [pool.submit(run_trial, case, arm, trial, args, meta) for case, arm, trial in jobs]
        for future in concurrent.futures.as_completed(futures):
            record = future.result()
            records.append(record)
            grade = record["grade"]
            flag = "PASS" if grade["pass"] else ("INFRA" if record["infra_error"] else "fail")
            print(f"{flag:5} {record['arm']:9} {record['case_id']} #{record['trial']} {','.join(grade.get('hard_fails', []))}")

    if args.split == "holdout":
        with HOLDOUT_LOG.open("a") as log:
            log.write(json.dumps({"date": datetime.date.today().isoformat(), "run_id": args.run_id,
                                  "milestone": args.milestone, "skill_sha": meta["skill_sha"],
                                  "case_set_sha": meta["case_set_sha"], "architect_model": args.model,
                                  "judge_model": args.judge_model}) + "\n")
    subprocess.run([sys.executable, str(ROOT / "report.py"), str(run_dir)], check=False)


if __name__ == "__main__":
    main()
