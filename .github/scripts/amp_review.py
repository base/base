#!/usr/bin/env python3
"""Checks whether Amp changed a pull request, and whether it had independent review.

Amp changed a PR if any of these is true:
  - an Amp workflow run was requested for it (runs are named "Amp: PR #<n>")
  - a commit on it was authored or committed by the Base Amp GitHub App
  - the app pushed to its branch (repository activity API)

Who requested Amp comes from GitHub Actions run history (the actor and
triggering actor of each run that wasn't skipped). The app can't edit or delete
runs, so Amp can't hide who asked for it.

Commands:
  has-amp-changes      writes amp_changes=true|false to $GITHUB_OUTPUT
  independent-review   fails unless the current head commit is approved by a
                       human who didn't request Amp on this PR. If Amp pushed
                       but no Amp run for this PR exists, two such approvals
                       are required.

Errors reading GitHub count as "Amp changed this PR", so both commands fail
closed.

Environment: GH_TOKEN, GH_REPO, PR_NUMBER, AMP_APP_SLUG (app slug without
"[bot]"), optionally AMP_APP_CLIENT_ID and AMP_WORKFLOW (default amp.yml).
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

API = os.environ.get("GITHUB_API_URL", "https://api.github.com")
# Run states that mean the Amp workflow did something for the PR. Skipped runs
# are other PR comments that didn't start with /amp.
RUN_STATUSES = [
    "queued", "in_progress", "waiting", "pending", "requested",
    "success", "failure", "cancelled", "timed_out", "action_required",
    "neutral", "stale", "startup_failure",
]


class GitHub:
    def __init__(self, token: str, repo: str):
        self.token = token
        self.repo = repo

    def get(self, path: str, params: dict | None = None):
        url = f"{API}/repos/{self.repo}/{path}"
        if params:
            url += "?" + urllib.parse.urlencode(params)
        request = urllib.request.Request(url, headers={
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        })
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response), response.headers.get("Link", "")

    def paginate(self, path: str, params: dict | None = None, key: str | None = None, limit: int = 2000):
        params = {**(params or {}), "per_page": 100, "page": 1}
        items: list = []
        while len(items) < limit:
            data, link = self.get(path, params)
            page = data[key] if key else data
            items.extend(page)
            if 'rel="next"' not in link or not page:
                break
            params["page"] += 1
        return items


def amp_runs(gh: GitHub, workflow: str, pr: dict) -> list[dict]:
    title = f"Amp: PR #{pr['number']}"
    since = pr["created_at"][:10]
    runs: dict[int, dict] = {}
    for status in RUN_STATUSES:
        try:
            page = gh.paginate(
                f"actions/workflows/{workflow}/runs",
                {"status": status, "created": f">={since}", "exclude_pull_requests": "true"},
                key="workflow_runs",
            )
        except urllib.error.HTTPError as error:
            if error.code == 404:  # The Amp workflow doesn't exist on this repo.
                return []
            raise
        for run in page:
            if run.get("display_title") == title:
                runs[run["id"]] = run
    return list(runs.values())


def requesters(runs: list[dict]) -> set[str]:
    found = set()
    for run in runs:
        for key in ("actor", "triggering_actor"):
            login = (run.get(key) or {}).get("login")
            if login:
                found.add(login.lower())
    return found


def app_touched(gh: GitHub, pr: dict, bot: str) -> bool:
    bot = bot.lower()
    for commit in gh.paginate(f"pulls/{pr['number']}/commits", limit=250):
        for key in ("author", "committer"):
            if ((commit.get(key) or {}).get("login") or "").lower() == bot:
                return True
    pushes = gh.paginate("activity", {"ref": f"refs/heads/{pr['head']['ref']}", "actor": bot})
    return bool(pushes)


def independent_approvals(reviews: list[dict], head_sha: str, excluded: set[str]) -> set[str]:
    latest: dict[str, dict] = {}
    for review in reviews:  # The API returns reviews oldest first.
        if review.get("state") in ("APPROVED", "CHANGES_REQUESTED", "DISMISSED"):
            latest[(review.get("user") or {}).get("login", "").lower()] = review
    return {
        login
        for login, review in latest.items()
        if review["state"] == "APPROVED"
        and review.get("commit_id") == head_sha
        and (review.get("user") or {}).get("type") == "User"
        and login not in excluded
    }


def evaluate(gh: GitHub, pr_number: int, slug: str, workflow: str) -> dict:
    pr, _ = gh.get(f"pulls/{pr_number}")
    runs = amp_runs(gh, workflow, pr)
    touched = bool(slug) and app_touched(gh, pr, f"{slug}[bot]")
    return {
        "pr": pr,
        "runs": runs,
        "changed": bool(runs) or touched,
        "requesters": requesters(runs),
    }


def main() -> int:
    command = sys.argv[1] if len(sys.argv) > 1 else ""
    if command not in ("has-amp-changes", "independent-review"):
        print(__doc__)
        return 2

    slug = os.environ.get("AMP_APP_SLUG", "").strip()
    if not slug and os.environ.get("AMP_APP_CLIENT_ID", "").strip():
        print("::error::AMP_GITHUB_APP_SLUG is not set but the Amp GitHub App is configured.")
        return 1
    gh = GitHub(os.environ["GH_TOKEN"], os.environ["GH_REPO"])
    pr_number = int(os.environ["PR_NUMBER"])
    workflow = os.environ.get("AMP_WORKFLOW", "amp.yml")

    try:
        result = evaluate(gh, pr_number, slug, workflow)
    except (urllib.error.URLError, KeyError, ValueError) as error:
        print(f"::error::Could not read PR #{pr_number} from GitHub: {error}")
        if command == "has-amp-changes":
            write_output("amp_changes", "true")
        return 1

    if command == "has-amp-changes":
        write_output("amp_changes", "true" if result["changed"] else "false")
        print(f"Amp changes on PR #{pr_number}: {result['changed']}")
        return 0

    if not result["changed"]:
        print(f"Amp has not changed PR #{pr_number}.")
        return 0

    pr = result["pr"]
    excluded = result["requesters"] | {f"{slug}[bot]".lower()}
    reviews = gh.paginate(f"pulls/{pr_number}/reviews")
    approvers = independent_approvals(reviews, pr["head"]["sha"], excluded)
    needed = 1 if result["requesters"] else 2
    names = ", ".join(sorted(result["requesters"])) or "unknown"
    print(f"Amp requested on PR #{pr_number} by: {names}")
    print(f"Independent approvals of {pr['head']['sha']}: {', '.join(sorted(approvers)) or 'none'}")
    if len(approvers) < needed:
        print(
            f"::error::Amp changed this PR. It needs {needed} approval(s) of the current head "
            f"from someone who didn't request Amp on it (requested by: {names})."
        )
        return 1
    return 0


def write_output(name: str, value: str) -> None:
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a") as handle:
            handle.write(f"{name}={value}\n")


if __name__ == "__main__":
    sys.exit(main())
