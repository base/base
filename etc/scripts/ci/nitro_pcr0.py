"""Fail closed on PCR0 drift and report mismatches from a trusted workflow."""

import argparse
import io
import json
import os
import re
import subprocess
import sys
import zipfile
from pathlib import Path
from urllib.parse import urlencode

EXPECTED_PATH = "etc/docker/nitro-enclave/pcr0.txt"
WORKFLOW_PATH = ".github/workflows/nitro-pcr0.yml"
MAX_ARTIFACT_BYTES = 16384


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def invalid_constant(value):
    raise ValueError(f"invalid JSON constant: {value}")


def read_json(data):
    return json.loads(
        data, object_pairs_hook=unique_object, parse_constant=invalid_constant
    )


def digest(value, label):
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{96}", value) is None:
        raise ValueError(f"{label} must be 96 lowercase hexadecimal characters")
    return value


def positive_integer(value, label):
    if type(value) is not int or value < 1:
        raise ValueError(f"invalid {label}")
    return value


def head_sha(value):
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{40}", value) is None:
        raise ValueError("invalid head SHA")
    return value


def check(args):
    args.result.unlink(missing_ok=True)
    expected = digest(args.expected.read_text().removesuffix("\n"), "expected PCR0")
    measurement = read_json(args.measurement.read_bytes())
    if not isinstance(measurement, dict) or not isinstance(
        measurement.get("Measurements"), dict
    ):
        raise TypeError("measurement must contain Measurements.PCR0")
    actual = digest(measurement["Measurements"].get("PCR0"), "measured PCR0")
    result = {
        "version": 1,
        "expected": expected,
        "actual": actual,
        "head_sha": head_sha(args.head_sha),
        "run_id": positive_integer(args.run_id, "run ID"),
        "run_attempt": positive_integer(args.run_attempt, "run attempt"),
    }
    args.result.write_text(json.dumps(result) + "\n")
    if expected != actual:
        print(
            f"PCR0 mismatch\nCommitted expected: {expected}\nMeasured actual:    {actual}\n"
            f"Review the enclave changes before updating {EXPECTED_PATH}.",
            file=sys.stderr,
        )
        return 1
    print(f"PCR0 matches: {actual}")
    return 0


def api(endpoint, *, body=None, raw=False):
    command = ["gh", "api", endpoint]
    if body is not None:
        command.extend(["--method", "POST", "--input", "-"])
    response = subprocess.run(
        command,
        input=json.dumps(body).encode() if body else None,
        capture_output=True,
        timeout=60,
        check=True,
    )
    if raw:
        if len(response.stdout) > MAX_ARTIFACT_BYTES:
            raise ValueError("artifact archive is too large")
        return response.stdout
    return read_json(response.stdout)


def artifact_report(repository, run):
    attempt = positive_integer(run["run_attempt"], "run attempt")
    artifacts = api(
        f"repos/{repository}/actions/runs/{run['id']}/artifacts?per_page=100"
    )
    matches = [
        artifact
        for artifact in artifacts["artifacts"]
        if artifact["name"] == f"nitro-pcr0-{attempt}"
    ]
    if not matches:
        print("No PCR0 result artifact; the build or measurement may have failed.")
        return None
    if len(matches) != 1:
        raise ValueError("ambiguous PCR0 artifacts")
    artifact = matches[0]
    artifact_id = positive_integer(artifact["id"], "artifact ID")
    size = positive_integer(artifact["size_in_bytes"], "artifact size")
    if artifact["expired"] or size > MAX_ARTIFACT_BYTES:
        raise ValueError("expired or oversized PCR0 artifact")
    data = api(f"repos/{repository}/actions/artifacts/{artifact_id}/zip", raw=True)
    # Never extract or execute files from the untrusted build.
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        entries = archive.infolist()
        if len(entries) != 1 or entries[0].filename != "result.json":
            raise ValueError("artifact must contain only result.json")
        if entries[0].file_size > MAX_ARTIFACT_BYTES:
            raise ValueError("PCR0 result is too large")
        report = read_json(archive.read(entries[0]))
    fields = {"version", "run_id", "run_attempt", "head_sha", "expected", "actual"}
    if (
        not isinstance(report, dict)
        or set(report) != fields
        or type(report["version"]) is not int
        or report["version"] != 1
    ):
        raise ValueError("invalid PCR0 result schema")
    digest(report["expected"], "expected PCR0")
    digest(report["actual"], "measured PCR0")
    head_sha(report["head_sha"])
    for field in ("run_id", "run_attempt"):
        positive_integer(report[field], field)
    if (report["run_id"], report["run_attempt"], report["head_sha"]) != (
        run["id"],
        run["run_attempt"],
        run["head_sha"],
    ):
        raise ValueError("PCR0 result does not belong to this run/head/attempt")
    return report


def pull_request(repository, run):
    head_repository = run["head_repository"]
    head_repository_id = positive_integer(head_repository["id"], "head repository ID")
    head_repository_name = head_repository["full_name"]
    if (
        not isinstance(head_repository_name, str)
        or re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", head_repository_name)
        is None
    ):
        raise ValueError("invalid head repository")
    if not isinstance(run["head_branch"], str) or not run["head_branch"]:
        raise ValueError("invalid head branch")
    associated = run["pull_requests"]
    if not associated:
        # Fork commits need not exist in the base repository's commit associations.
        query = urlencode(
            {
                "state": "open",
                "head": f"{head_repository_name.split('/')[0]}:{run['head_branch']}",
                "per_page": 100,
            }
        )
        associated = api(f"repos/{repository}/pulls?{query}")
        associated = [
            pr
            for pr in associated
            if pr["state"] == "open"
            and pr["head"]["sha"] == run["head_sha"]
            and pr["head"]["ref"] == run["head_branch"]
            and pr["base"]["repo"]["full_name"] == repository
            and pr["head"]["repo"]["full_name"] == head_repository_name
            and pr["head"]["repo"]["id"] == head_repository_id
        ]
        if not associated:
            print("No current PR for this run; skipping stale result.")
            return None
    if len(associated) != 1:
        raise ValueError("ambiguous PR association")
    number = positive_integer(associated[0]["number"], "PR number")
    pr = api(f"repos/{repository}/pulls/{number}")
    if pr["number"] != number or pr["base"]["repo"]["full_name"] != repository:
        raise ValueError("PR does not belong to this repository")
    if (
        pr["state"] != "open"
        or pr["head"]["sha"] != run["head_sha"]
        or pr["head"]["ref"] != run["head_branch"]
    ):
        print("PR is closed or has a newer head; skipping stale result.")
        return None
    if (
        pr["head"]["repo"]["full_name"] != head_repository_name
        or pr["head"]["repo"]["id"] != head_repository_id
    ):
        raise ValueError("PR head repository does not match the workflow run")
    return pr


def comment(args):
    event = read_json(args.event.read_bytes())
    repository = event["repository"]["full_name"]
    if (
        not isinstance(repository, str)
        or re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is None
    ):
        raise ValueError("invalid repository")
    if os.environ.get("GITHUB_REPOSITORY", repository) != repository:
        raise ValueError("event repository does not match workflow repository")
    origin = event["workflow_run"]
    run_id = positive_integer(origin["id"], "run ID")
    run = api(f"repos/{repository}/actions/runs/{run_id}")
    if run["run_attempt"] != origin["run_attempt"]:
        print("A newer run attempt exists; skipping stale result.")
        return 0
    if run["id"] != run_id or run["head_sha"] != origin["head_sha"]:
        raise ValueError("workflow run does not match the event")
    head_sha(run["head_sha"])
    if run["path"] != WORKFLOW_PATH:
        raise ValueError("unexpected originating workflow")
    if (
        run["event"] != "pull_request"
        or run["status"] != "completed"
        or run["conclusion"] != "failure"
    ):
        return 0
    report = artifact_report(repository, run)
    if report is None or report["actual"] == report["expected"]:
        return 0
    pr = pull_request(repository, run)
    if pr is None:
        return 0
    body = (
        "<!-- nitro-pcr0-mismatch -->\n## Nitro enclave PCR0 mismatch\n\n"
        f"Commit: `{run['head_sha']}` · [Build run](https://github.com/{repository}/actions/runs/{run_id})\n\n"
        f"**Committed expected**\n```text\n{report['expected']}\n```\n"
        f"**Measured actual**\n```text\n{report['actual']}\n```\n\n"
        "The PCR0 check failed. If this enclave change is intentional, review the build inputs "
        f"and replace the value in `{EXPECTED_PATH}` with:\n\n"
        f"```text\n{report['actual']}\n```\n"
        "Do not update the expected measurement merely to silence an unexplained mismatch."
    )
    api(f"repos/{repository}/issues/{pr['number']}/comments", body={"body": body})
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    gate = commands.add_parser("check")
    gate.add_argument("--expected", type=Path, default=Path(EXPECTED_PATH))
    gate.add_argument("--measurement", type=Path, required=True)
    gate.add_argument("--result", type=Path, required=True)
    gate.add_argument("--head-sha", required=True)
    gate.add_argument("--run-id", type=int, required=True)
    gate.add_argument("--run-attempt", type=int, required=True)
    reporter = commands.add_parser("comment")
    reporter.add_argument("--event", type=Path, required=True)
    args = parser.parse_args()
    try:
        return check(args) if args.command == "check" else comment(args)
    except subprocess.CalledProcessError as error:
        print(
            f"PCR0 comment failed: {error.stderr.decode(errors='replace').strip()}",
            file=sys.stderr,
        )
    except (
        OSError,
        ValueError,
        KeyError,
        TypeError,
        zipfile.BadZipFile,
        subprocess.TimeoutExpired,
    ) as error:
        print(f"PCR0 validation failed: {error}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
