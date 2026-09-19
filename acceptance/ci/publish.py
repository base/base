#!/usr/bin/env python3
"""Validate an acceptance aggregate and safely upsert its trusted PR summary.

Comment ownership is deliberately narrow: Depot's code-access bot (or BOT_LOGIN) must
match, the author must be a Bot, and the body must start with MARKER. A different
bot using the marker is never overwritten.
"""

import argparse
import datetime
import json
import os
import re
import urllib.request

MARKER = "<!-- acceptance-results -->"
META_PREFIX = "<!-- acceptance-results-meta:"
MAX_PAGES = 20
MAX_BODY = 60_000
MAX_FILE = 20 * 1024 * 1024
MAX_SCENARIOS = 100
MAX_CHECKS = 1000
MAX_TEXT = 2_000
SHA = re.compile(r"^[0-9a-f]{40}$")
ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}$")
EVIDENCE = re.compile(r"^(?!/)(?!.*(?:^|/)\.\.(?:/|$))[A-Za-z0-9._/-]{1,300}$")
STATUSES = {"passed", "failed", "error", "blocked", "cancelled"}


class ValidationError(ValueError):
    pass


class Api:
    def __init__(self, repo, token):
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
            raise ValidationError("invalid repository")
        self.root = f"https://api.github.com/repos/{repo}"
        self.api_root = "https://api.github.com"
        self.token = token

    def request(self, method, path, body=None, absolute=False):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(
            (self.api_root if absolute else self.root) + path, data=data, method=method
        )
        for key, value in {
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        }.items():
            req.add_header(key, value)
        if data:
            req.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(req, timeout=20) as response:
            return json.load(response)


def _object(value, keys, where):
    if not isinstance(value, dict) or set(value) != set(keys):
        raise ValidationError(f"invalid {where} fields")


def _text(value, where, empty=True):
    if not isinstance(value, str) or len(value) > MAX_TEXT or (not empty and not value):
        raise ValidationError(f"invalid {where}")
    return value


def _status(value):
    if value not in STATUSES:
        raise ValidationError("invalid status")
    return value


def _value(value, depth=0):
    if depth > 8:
        raise ValidationError("JSON value is too deep")
    if isinstance(value, str):
        _text(value, "JSON string")
    elif value is None or isinstance(value, (bool, int, float)):
        pass
    elif isinstance(value, list):
        if len(value) > 200:
            raise ValidationError("JSON array is too large")
        for item in value:
            _value(item, depth + 1)
    elif isinstance(value, dict):
        if len(value) > 200:
            raise ValidationError("JSON object is too large")
        for key, item in value.items():
            _text(key, "JSON key", False)
            _value(item, depth + 1)
    else:
        raise ValidationError("invalid JSON value")


def load_json(path):
    if not os.path.isfile(path) or os.path.getsize(path) > MAX_FILE:
        raise ValidationError("missing or oversized result")
    with open(path, encoding="utf-8") as source:
        return json.load(source)


def manifest_ids(manifest, run_id, tested_sha):
    if (
        not isinstance(manifest, dict)
        or manifest.get("schema_version") != 1
        or manifest.get("run_id") != run_id
        or manifest.get("tested_sha") != tested_sha
    ):
        raise ValidationError("manifest provenance mismatch")
    scenarios = manifest.get("scenarios")
    if (
        not isinstance(scenarios, list)
        or not scenarios
        or len(scenarios) > MAX_SCENARIOS
    ):
        raise ValidationError("invalid manifest scenarios")
    output = {}
    for scenario in scenarios:
        if not isinstance(scenario, dict) or set(scenario) not in (
            {"id", "checks"},
            {"id", "check_ids"},
        ):
            raise ValidationError("invalid manifest scenario")
        sid = scenario.get("id")
        checks = scenario.get("checks", scenario.get("check_ids"))
        if (
            not isinstance(sid, str)
            or not ID.fullmatch(sid)
            or sid in output
            or not isinstance(checks, list)
            or not checks
            or len(checks) > MAX_CHECKS
        ):
            raise ValidationError("invalid manifest IDs")
        ids = []
        for check in checks:
            cid = (
                check.get("id")
                if isinstance(check, dict) and set(check) == {"id"}
                else check
            )
            if not isinstance(cid, str) or not ID.fullmatch(cid) or cid in ids:
                raise ValidationError("invalid manifest check IDs")
            ids.append(cid)
        output[sid] = ids
    return output


def validate_result(result, expected, run_id, tested_sha):
    _object(
        result,
        ["schema_version", "run_id", "tested_sha", "started_at_unix_ms", "scenarios"],
        "result",
    )
    if (
        result["schema_version"] != 1
        or result["run_id"] != run_id
        or result["tested_sha"] != tested_sha
        or not SHA.fullmatch(tested_sha)
    ):
        raise ValidationError("result provenance mismatch")
    if (
        not isinstance(result["started_at_unix_ms"], int)
        or result["started_at_unix_ms"] < 0
    ):
        raise ValidationError("invalid result timestamp")
    scenarios = result["scenarios"]
    if (
        not isinstance(scenarios, list)
        or not scenarios
        or len(scenarios) > MAX_SCENARIOS
    ):
        raise ValidationError("invalid result scenarios")
    seen = {}
    scenario_keys = [
        "id",
        "status",
        "duration_ms",
        "config",
        "stages",
        "checks",
        "samples",
        "forks",
        "diagnostics",
        "reproduction",
    ]
    check_keys = [
        "id",
        "kind",
        "status",
        "duration_ms",
        "expected",
        "observed",
        "message",
        "next_step",
        "samples",
        "rpc_errors",
        "evidence",
    ]
    for scenario in scenarios:
        _object(scenario, scenario_keys, "scenario")
        sid = scenario["id"]
        if sid in seen or sid not in expected:
            raise ValidationError("unexpected or duplicate scenario")
        _status(scenario["status"])
        _text(scenario["reproduction"], "reproduction")
        _value(scenario["config"])
        for key in ("stages", "forks", "diagnostics"):
            if not isinstance(scenario[key], list) or len(scenario[key]) > 500:
                raise ValidationError(f"invalid {key}")
            _value(scenario[key])
        if (
            not isinstance(scenario["samples"], list)
            or len(scenario["samples"]) > 50_000
        ):
            raise ValidationError("invalid samples")
        for stage in scenario["stages"]:
            _object(stage, ["id", "status", "duration_ms", "message"], "stage")
            _status(stage["status"])
            _text(stage["id"], "stage id")
            _text(stage["message"], "stage message")
        for sample in scenario["samples"]:
            _object(
                sample,
                ["endpoint", "elapsed_ms", "number", "timestamp", "hash"],
                "sample",
            )
            _text(sample["endpoint"], "sample endpoint")
        for fork in scenario["forks"]:
            _object(
                fork,
                [
                    "name",
                    "chain",
                    "activation_timestamp",
                    "observed_block",
                    "observed_elapsed_ms",
                ],
                "fork",
            )
            _text(fork["name"], "fork name")
            _text(fork["chain"], "fork chain")
        if any(not isinstance(item, str) for item in scenario["diagnostics"]):
            raise ValidationError("invalid diagnostics")
        if (
            not isinstance(scenario["checks"], list)
            or len(scenario["checks"]) > MAX_CHECKS
        ):
            raise ValidationError("invalid checks")
        ids = []
        for check in scenario["checks"]:
            _object(check, check_keys, "check")
            cid = check["id"]
            if cid in ids or not isinstance(cid, str):
                raise ValidationError("duplicate check")
            ids.append(cid)
            _status(check["status"])
            for key in ("kind", "message", "next_step"):
                _text(check[key], key)
            _value(check["expected"])
            _value(check["observed"])
            if (
                not isinstance(check["evidence"], list)
                or len(check["evidence"]) > 50
                or any(
                    not isinstance(p, str) or not EVIDENCE.fullmatch(p)
                    for p in check["evidence"]
                )
            ):
                raise ValidationError("invalid evidence path")
        if ids != expected[sid]:
            raise ValidationError("check IDs differ from manifest")
        # Never trust the supplied aggregate status, including after cleanup failure.
        statuses = [
            record["status"] for record in scenario["stages"] + scenario["checks"]
        ]
        scenario["status"] = next(
            (
                status
                for status in ("error", "cancelled", "failed", "blocked")
                if status in statuses
            ),
            "passed",
        )
        seen[sid] = scenario
    if list(seen) != list(expected):
        raise ValidationError("scenario IDs differ from manifest")
    return result


def md(value):
    if not isinstance(value, str):
        value = json.dumps(
            value, ensure_ascii=True, separators=(",", ":"), sort_keys=True
        )
    value = value.replace("\r", " ").replace("\n", " ")
    value = re.sub(r"https?://\S+", "[link removed]", value)
    value = (
        value.replace("@", "＠")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace("`", "&#96;")
        .replace("|", "&#124;")
    )
    value = (
        value.replace("[", "&#91;")
        .replace("]", "&#93;")
        .replace("(", "&#40;")
        .replace(")", "&#41;")
    )
    return value[:MAX_TEXT]


def comment_body(result, checks_url, metadata):
    counts = {s: 0 for s in STATUSES}
    for scenario in result["scenarios"]:
        for check in scenario["checks"]:
            counts[check["status"]] += 1
    passed = all(
        s["status"] == "passed" and all(c["status"] == "passed" for c in s["checks"])
        for s in result["scenarios"]
    )
    out = f"{MARKER}\n{META_PREFIX}{json.dumps(metadata, separators=(',', ':'), sort_keys=True)} -->\n## Acceptance: {'✅ Passed' if passed else '❌ Not passed'}\n\nRevision `{md(result['tested_sha'])}` · [PR checks]({checks_url})\n\n| Passed | Failed | Error | Blocked | Cancelled |\n|---:|---:|---:|---:|---:|\n| {counts['passed']} | {counts['failed']} | {counts['error']} | {counts['blocked']} | {counts['cancelled']} |\n\n"
    for scenario in result["scenarios"]:
        section = f"<details{' open' if scenario['status'] != 'passed' else ''}><summary>{md(scenario['id'])} — {md(scenario['status'])}</summary>\n\n"
        for stage in scenario.get("stages", []):
            if stage["status"] != "passed":
                section += f"**Lifecycle: {md(stage['id'])} — {md(stage['status'])}**: {md(stage['message'])}\n\n"
        for check in scenario["checks"]:
            section += f"**{md(check['id'])}** — {md(check['status'])}\n\n- Expected: `{md(check['expected'])}`\n- Observed: `{md(check['observed'])}`\n- Message: {md(check['message'])}\n- Next step: {md(check['next_step'])}\n\n"
        section += f"Reproduction: `{md(scenario['reproduction'])}`\n\n</details>\n\n"
        if len((out + section).encode()) > MAX_BODY:
            raise ValidationError("comment body exceeds bound")
        out += section
    return out


def parse_meta(body):
    if not body.startswith(MARKER + "\n" + META_PREFIX):
        return None
    end = body.find(" -->", len(MARKER) + 1)
    try:
        return json.loads(body[body.find(META_PREFIX) + len(META_PREFIX) : end])
    except (ValueError, TypeError):
        return None


def publish(api, *, pr, head, base, run_id, attempt, started_at, body, bot_login):
    def current():
        item = api.request("GET", f"/pulls/{pr}")
        return (
            item.get("head", {}).get("sha") == head
            and item.get("base", {}).get("sha") == base
        )

    if not current():
        return False
    owned = None
    exhausted = True
    for page in range(1, MAX_PAGES + 1):
        comments = api.request("GET", f"/issues/{pr}/comments?per_page=100&page={page}")
        for comment in comments:
            if (
                comment.get("user", {}).get("type") == "Bot"
                and comment.get("user", {}).get("login") == bot_login
                and comment.get("body", "").startswith(MARKER)
            ):
                if owned is not None:
                    raise ValidationError("duplicate owned comments")
                owned = comment
        if len(comments) < 100:
            exhausted = False
            break
    if exhausted:
        raise ValidationError("comment pagination cap reached")
    if owned:
        old = parse_meta(owned["body"])
        if not old:
            raise ValidationError("owned marker has invalid metadata")
        old_attempt = old.get("attempt")
        old_started = old.get("started_at")
        if old.get("run_id") == run_id:
            if not isinstance(old_attempt, int) or old_attempt > attempt:
                return False
        elif not isinstance(old_started, str) or old_started >= started_at:
            return False
    if not current():
        return False
    path = f"/issues/comments/{owned['id']}" if owned else f"/issues/{pr}/comments"
    api.request("PATCH" if owned else "POST", path, {"body": body})
    return True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--event", required=True)
    parser.add_argument("--result", required=True)
    parser.add_argument("--expected", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--attempt", required=True, type=int)
    parser.add_argument("--started-at", required=True)
    args = parser.parse_args()
    with open(args.event, encoding="utf-8") as source:
        event = json.load(source)
    pr = event["pull_request"]
    repo = os.environ["GITHUB_REPOSITORY"]
    token = os.environ["GITHUB_TOKEN"]
    tested = os.environ["TESTED_SHA"]
    try:
        expected = manifest_ids(load_json(args.expected), args.run_id, tested)
        result = validate_result(load_json(args.result), expected, args.run_id, tested)
    except (ValidationError, OSError, ValueError, TypeError, KeyError):
        result = {
            "tested_sha": tested,
            "scenarios": [
                {
                    "id": "report-integrity",
                    "status": "error",
                    "reproduction": "Rerun the acceptance workflow after inspecting its job logs.",
                    "checks": [
                        {
                            "id": "complete-results",
                            "status": "error",
                            "expected": "Complete, valid artifacts matching the current run and revision",
                            "observed": "Results were missing or could not be validated",
                            "message": "Infrastructure or artifact integrity prevented publication of the full breakdown.",
                            "next_step": "Open PR checks and download the acceptance report artifact.",
                        }
                    ],
                }
            ],
        }
    datetime.datetime.fromisoformat(args.started_at.replace("Z", "+00:00"))
    api = Api(repo, token)
    bot = os.environ.get("BOT_LOGIN") or "depot-code-access[bot]"
    metadata = {
        "attempt": args.attempt,
        "run_id": args.run_id,
        "started_at": args.started_at,
    }
    url = f"https://github.com/{repo}/pull/{int(pr['number'])}/checks"
    body = comment_body(result, url, metadata)
    ok = publish(
        api,
        pr=int(pr["number"]),
        head=pr["head"]["sha"],
        base=pr["base"]["sha"],
        run_id=args.run_id,
        attempt=args.attempt,
        started_at=args.started_at,
        body=body,
        bot_login=bot,
    )
    print("published" if ok else "stale run; publication skipped")


if __name__ == "__main__":
    main()
