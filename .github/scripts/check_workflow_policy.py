#!/usr/bin/env python3
"""Checks GitHub Actions workflows against Base's security policy.

All workflows:
  - must not use `pull_request_target` or `workflow_run` triggers
  - must pin third-party actions to a full commit SHA
  - must reference secrets by literal name (no `toJSON(secrets)`,
    `secrets[format(...)]` or bare `secrets`)
  - may use the Amp GitHub App key (`secrets.AMP_GITHUB_APP_PRIVATE_KEY`) only
    as the `private-key` input of `actions/create-github-app-token`, scoped with
    `repositories` and only contents, pull-requests or issues permissions

Workflows that use the Amp bot key (`secrets.AMP_API_KEY`) must also:
  - only trigger on `issue_comment`, `workflow_dispatch` or `pull_request`
  - set top-level `permissions`
  - gate `issue_comment` runs behind a job that checks `author_association`
  - run each job that uses the key:
      - on a Depot runner (`depot-*`), never a self-hosted runner
      - with explicit, read-only job `permissions`
      - with no secrets other than the Amp GitHub App key, and no
        `secrets: inherit`
      - with `disable-sudo-and-containers: true` on harden-runner as the first
        step, if it mints a GitHub App token (keeps the private key out of reach)
      - with `persist-credentials: false` on every checkout
      - only after the authorization job, when `issue_comment` is a trigger

Usage: check_workflow_policy.py [WORKFLOW_DIR]
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

import yaml

AMP_SECRET = "AMP_API_KEY"
APP_KEY_SECRET = "AMP_GITHUB_APP_PRIVATE_KEY"
APP_TOKEN_ACTION = "actions/create-github-app-token@"
HARDEN_RUNNER = "step-security/harden-runner@"
ALLOWED_APP_PERMISSIONS = {"contents", "pull-requests", "issues"}
BANNED_TRIGGERS = {"pull_request_target", "workflow_run"}
AMP_ALLOWED_TRIGGERS = {"issue_comment", "workflow_dispatch", "pull_request"}
# Matches secrets.NAME and secrets['NAME'] / secrets["NAME"].
SECRET_REF = re.compile(
    r"""secrets\s*(?:\.\s*([A-Za-z_][A-Za-z0-9_]*)|\[\s*['"]([A-Za-z_][A-Za-z0-9_]*)['"]\s*\])"""
)
# Any other use of the secrets context (toJSON(secrets), secrets[format(...)],
# bare secrets) could expose the Amp key without naming it.
DYNAMIC_SECRETS = re.compile(r"""\bsecrets\b(?!\s*\.\s*[A-Za-z_]|\s*\[\s*['"][A-Za-z_][A-Za-z0-9_]*['"]\s*\])""")
PINNED = re.compile(r"^[^@\s]+@[0-9a-f]{40}$")


def triggers(workflow: dict) -> set[str]:
    # PyYAML parses the bare key `on` as boolean True.
    on = workflow.get("on", workflow.get(True))
    if isinstance(on, str):
        return {on}
    if isinstance(on, list):
        return set(on)
    if isinstance(on, dict):
        return set(on)
    return set()


def walk(node):
    if isinstance(node, dict):
        for value in node.values():
            yield from walk(value)
    elif isinstance(node, list):
        for value in node:
            yield from walk(value)
    else:
        yield node


def secrets_used(node) -> set[str]:
    found: set[str] = set()
    for value in walk(node):
        if isinstance(value, str):
            for dotted, indexed in SECRET_REF.findall(value):
                found.add(dotted or indexed)
    return found


def expressions(node):
    for value in walk(node):
        if isinstance(value, str):
            yield from re.findall(r"\$\{\{(.*?)\}\}", value, re.S)


def uses_refs(workflow: dict):
    for job in (workflow.get("jobs") or {}).values():
        if isinstance(job.get("uses"), str):
            yield job["uses"]
        for step in job.get("steps") or []:
            if isinstance(step.get("uses"), str):
                yield step["uses"]


def is_read_only(permissions) -> bool:
    if permissions in ("read-all", {}):
        return True
    if isinstance(permissions, dict):
        return all(value in ("read", "none") for value in permissions.values())
    return False


def check_amp_workflow(workflow: dict, errors: list[str]) -> None:
    on = triggers(workflow)
    extra = on - AMP_ALLOWED_TRIGGERS
    if extra:
        errors.append(f"uses {AMP_SECRET} with disallowed triggers: {sorted(extra)}")
    if "permissions" not in workflow:
        errors.append("uses Amp but does not set top-level permissions")

    jobs = workflow.get("jobs") or {}
    authorize_jobs = {
        name
        for name, job in jobs.items()
        if "author_association" in str(job.get("if", ""))
    }
    if "issue_comment" in on and not authorize_jobs:
        errors.append("issue_comment trigger without a job that checks author_association")

    for name, job in jobs.items():
        used = secrets_used(job)
        if AMP_SECRET not in used:
            continue
        where = f"job '{name}'"
        runs_on = job.get("runs-on")
        if not (isinstance(runs_on, str) and runs_on.startswith("depot-")):
            errors.append(f"{where} uses Amp but runs-on is not a depot-* runner: {runs_on!r}")
        if "uses" in job:
            errors.append(f"{where} uses Amp through a reusable workflow")
        if job.get("secrets") == "inherit":
            errors.append(f"{where} uses Amp with 'secrets: inherit'")
        other = used - {AMP_SECRET, APP_KEY_SECRET}
        if other:
            errors.append(f"{where} uses Amp alongside {len(other)} other secret(s)")
        if not is_read_only(job.get("permissions")):
            errors.append(f"{where} uses Amp without explicit read-only permissions")
        for step in job.get("steps") or []:
            if str(step.get("uses", "")).startswith("actions/checkout@"):
                if (step.get("with") or {}).get("persist-credentials") is not False:
                    errors.append(f"{where} checks out code without persist-credentials: false")
        if APP_KEY_SECRET in used:
            steps = job.get("steps") or []
            first = steps[0] if steps else {}
            if not (
                str(first.get("uses", "")).startswith(HARDEN_RUNNER)
                and (first.get("with") or {}).get("disable-sudo-and-containers") is True
            ):
                errors.append(
                    f"{where} mints an app token next to Amp without harden-runner "
                    "disable-sudo-and-containers: true as its first step"
                )
        if "issue_comment" in on:
            needs = job.get("needs") or []
            needs = [needs] if isinstance(needs, str) else needs
            if not authorize_jobs.intersection(needs):
                errors.append(f"{where} uses Amp without needing an author_association check")


def check_app_key(workflow: dict, errors: list[str]) -> None:
    for name, job in (workflow.get("jobs") or {}).items():
        where = f"job '{name}'"
        if APP_KEY_SECRET in secrets_used({k: v for k, v in job.items() if k != "steps"}):
            errors.append(f"{where} uses {APP_KEY_SECRET} outside a step")
        for step in job.get("steps") or []:
            if APP_KEY_SECRET not in secrets_used(step):
                continue
            inputs = step.get("with") or {}
            if not str(step.get("uses", "")).startswith(APP_TOKEN_ACTION):
                errors.append(f"{where} uses {APP_KEY_SECRET} outside actions/create-github-app-token")
                continue
            if APP_KEY_SECRET not in secrets_used(inputs.get("private-key", "")) or any(
                APP_KEY_SECRET in secrets_used(v) for k, v in step.items() if k != "with"
            ):
                errors.append(f"{where} passes {APP_KEY_SECRET} other than as private-key")
            if not inputs.get("repositories"):
                errors.append(f"{where} mints an app token without scoping repositories")
            perms = {k[len("permission-"):]: v for k, v in inputs.items() if k.startswith("permission-")}
            if not perms:
                errors.append(f"{where} mints an app token without explicit permission-* inputs")
            bad = sorted(set(perms) - ALLOWED_APP_PERMISSIONS)
            if bad:
                errors.append(f"{where} mints an app token with disallowed permissions: {bad}")


def check_file(path: Path) -> list[str]:
    workflow = yaml.safe_load(path.read_text()) or {}
    errors: list[str] = []

    banned = triggers(workflow) & BANNED_TRIGGERS
    if banned:
        errors.append(f"uses banned triggers: {sorted(banned)}")

    if any(DYNAMIC_SECRETS.search(expr) for expr in expressions(workflow)):
        errors.append("references secrets dynamically; use secrets.NAME")

    for ref in uses_refs(workflow):
        if ref.startswith(("./", "docker://")):
            continue
        if not PINNED.match(ref):
            errors.append(f"action not pinned to a full commit SHA: {ref}")

    if AMP_SECRET in secrets_used(workflow):
        check_amp_workflow(workflow, errors)
    if APP_KEY_SECRET in secrets_used(workflow):
        check_app_key(workflow, errors)

    return errors


def main() -> int:
    root = Path(sys.argv[1] if len(sys.argv) > 1 else ".github/workflows")
    failed = False
    for path in sorted([*root.glob("*.yml"), *root.glob("*.yaml")]):
        for error in check_file(path):
            print(f"::error file={path}::{path.name}: {error}")
            failed = True
    if not failed:
        print("All workflows pass the workflow security policy.")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
