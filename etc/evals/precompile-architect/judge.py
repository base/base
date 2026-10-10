#!/usr/bin/env python3
"""LLM judge for precompile-architect rubric items.

- One isolated call per rubric item, so one criterion cannot bias another.
- A fixed judge model, separate from the architect, called directly on the Messages API at
  temperature 0. The model id is recorded with every verdict.
- Blind to arm: the prompt never says whether the plan came from treatment or control.
- Sees the ticket, fork_state, expected plan, grader notes, one criterion, and the full plan.
- Returns yes, no or unknown. Transport errors and malformed replies are retried up to three
  times; an item still in error after that blocks the trial from passing if it is critical.
- Returns yes, no or unknown. unknown means the plan never addresses the criterion; it is
  never folded into no, and never counted as a pass.

Needs ANTHROPIC_BASE_URL and ANTHROPIC_AUTH_TOKEN (or ANTHROPIC_API_KEY). Extra gateway headers
are read from ANTHROPIC_CUSTOM_HEADERS, one "Name: value" per line.

Usage: python3 judge.py <case.json> <plan-text-file | transcript.json> [--model M]
"""

import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from evallib import NETWORKS, PROMPTS, load_json  # noqa: E402

DEFAULT_JUDGE_MODEL = "claude-opus-5"
MAX_PLAN_CHARS = 60_000
REQUEST_TIMEOUT_SECS = 180
VERDICTS = ("yes", "no", "unknown")
# Transport errors and empty or malformed replies are retried; a real verdict never is.
MAX_ATTEMPTS = 3


def fork_table(state):
    rows = ["| Network | Live | Scheduled |", "|---|---|---|"]
    for network in NETWORKS:
        live = ", ".join(state[network]["live"]) or "none"
        scheduled = ", ".join(state[network]["scheduled"]) or "none"
        rows.append(f"| {network} | {live} | {scheduled} |")
    return "\n".join(rows)


def _headers():
    token = os.environ.get("ANTHROPIC_AUTH_TOKEN")
    key = os.environ.get("ANTHROPIC_API_KEY")
    if not (token or key) or not os.environ.get("ANTHROPIC_BASE_URL"):
        raise SystemExit("judge needs ANTHROPIC_BASE_URL and ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY")
    headers = {"anthropic-version": "2023-06-01", "content-type": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    else:
        headers["x-api-key"] = key
    for line in os.environ.get("ANTHROPIC_CUSTOM_HEADERS", "").splitlines():
        name, sep, value = line.partition(":")
        if sep:
            headers[name.strip()] = value.strip()
    return headers


def ask(prompt, model):
    body = {"model": model, "max_tokens": 600, "temperature": 0, "messages": [{"role": "user", "content": prompt}]}
    request = urllib.request.Request(
        os.environ["ANTHROPIC_BASE_URL"].rstrip("/") + "/v1/messages",
        data=json.dumps(body).encode(), headers=_headers(),
    )
    with urllib.request.urlopen(request, timeout=REQUEST_TIMEOUT_SECS) as response:
        payload = json.load(response)
    return "".join(block.get("text", "") for block in payload.get("content", [])), payload.get("model", model)


def parse_verdict(reply):
    match = re.search(r"\{.*\}", reply, re.S)
    try:
        verdict = json.loads(match.group(0)) if match else None
    except ValueError:
        verdict = None
    if not isinstance(verdict, dict) or verdict.get("verdict") not in VERDICTS:
        return {"verdict": "error", "evidence": "", "reason": f"unparseable reply: {reply[:200]!r}"}
    return {"verdict": verdict["verdict"], "evidence": str(verdict.get("evidence", "")), "reason": str(verdict.get("reason", ""))}


def judge_case(case, plan_text, model=DEFAULT_JUDGE_MODEL):
    template = (PROMPTS / "judge.md").read_text()
    fields = {
        "ticket": case["ticket"],
        "fork_table": fork_table(case["fork_state"]),
        "expected": json.dumps(case["expected"], indent=2),
        "notes": case["notes"],
        "plan": plan_text[:MAX_PLAN_CHARS],
    }
    items, served_model = {}, model
    for item in case["rubric"]:
        prompt = template.format(check=item["check"], **fields)
        for attempt in range(1, MAX_ATTEMPTS + 1):
            try:
                reply, served_model = ask(prompt, model)
                verdict = parse_verdict(reply)
            except (urllib.error.URLError, TimeoutError, ValueError) as exc:
                verdict = {"verdict": "error", "evidence": "", "reason": str(exc)}
            if verdict["verdict"] != "error":
                break
            time.sleep(2 * attempt)
        verdict["attempts"] = attempt
        verdict["critical"] = item["critical"]
        items[item["id"]] = verdict
    return {
        "judge_model": served_model,
        "temperature": 0,
        "items": items,
        "unknown": sum(v["verdict"] == "unknown" for v in items.values()),
        "errors": sum(v["verdict"] == "error" for v in items.values()),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("case")
    parser.add_argument("plan_text")
    parser.add_argument("--model", default=DEFAULT_JUDGE_MODEL)
    args = parser.parse_args()
    text = Path(args.plan_text).read_text()
    if args.plan_text.endswith(".json"):
        text = json.loads(text).get("final_text") or json.loads(text)["payload"].get("result", "")
    print(json.dumps(judge_case(load_json(args.case), text, args.model), indent=2))


if __name__ == "__main__":
    main()
