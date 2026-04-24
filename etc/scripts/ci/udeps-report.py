#!/usr/bin/env python3
"""Helpers for the scheduled cargo-udeps issue workflow."""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path


class CommandError(RuntimeError):
    """Raised when a workflow helper command fails."""


def parse_args() -> argparse.Namespace:
    """Parse CLI arguments."""
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    render = subparsers.add_parser(
        "render",
        help="Render cargo-udeps JSON into an issue body and workflow outputs.",
    )
    render.add_argument("--json-path", type=Path, required=True)
    render.add_argument("--stderr-path", type=Path, required=True)
    render.add_argument("--issue-body-path", type=Path, required=True)
    render.add_argument("--github-output", type=Path, required=True)
    render.add_argument("--status", type=int, required=True)
    render.add_argument("--server-url", required=True)
    render.add_argument("--repository", required=True)
    render.add_argument("--run-id", required=True)

    find_open_issue = subparsers.add_parser(
        "find-open-issue",
        help="Find an open issue number matching an exact title from JSON on stdin.",
    )
    find_open_issue.add_argument("--title", required=True)

    return parser.parse_args()


def render_report(args: argparse.Namespace) -> int:
    """Render cargo-udeps JSON into workflow artifacts."""
    stdout = args.json_path.read_text(encoding="utf-8") if args.json_path.exists() else ""
    stderr = args.stderr_path.read_text(encoding="utf-8") if args.stderr_path.exists() else ""
    if not stdout.strip():
        sys.stderr.write(stderr)
        return args.status or 1

    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as exc:
        sys.stderr.write(stderr)
        raise CommandError(f"Invalid cargo-udeps JSON in {args.json_path}") from exc

    unused = payload.get("unused_deps", {})
    has_findings = bool(unused)

    lines = [
        "## Summary",
        "",
        "`cargo +nightly udeps --locked --workspace --all-features --all-targets --output json` reported unused dependencies.",
        "",
        f"- Updated at: {datetime.now(timezone.utc).isoformat()}",
        f"- Workflow run: {args.server_url}/{args.repository}/actions/runs/{args.run_id}",
        f"- cargo-udeps exit status: `{args.status}`",
        "",
        "## Findings",
        "",
    ]

    if args.status != 0:
        lines.extend(
            [
                "> Warning: `cargo udeps` exited non-zero after emitting JSON output. Treat the findings below as incomplete until the workflow failure is investigated.",
                "",
            ]
        )

    for package, details in sorted(unused.items()):
        package_name = package.split(" ", 1)[0]
        lines.append(f"### `{package_name}`")
        normal = details.get("normal", [])
        development = details.get("development", [])
        build = details.get("build", [])
        if normal:
            lines.append(f"- `dependencies`: {', '.join(f'`{dep}`' for dep in normal)}")
        if development:
            lines.append(f"- `dev-dependencies`: {', '.join(f'`{dep}`' for dep in development)}")
        if build:
            lines.append(f"- `build-dependencies`: {', '.join(f'`{dep}`' for dep in build)}")
        lines.append("")

    args.issue_body_path.write_text("\n".join(lines), encoding="utf-8")

    with args.github_output.open("a", encoding="utf-8") as fh:
        fh.write(f"has_findings={'true' if has_findings else 'false'}\n")
        fh.write(f"finding_count={len(unused)}\n")
        fh.write(f"command_failed={'true' if args.status != 0 else 'false'}\n")
        fh.write(f"command_status={args.status}\n")

    return 0


def find_open_issue(args: argparse.Namespace) -> int:
    """Print the number of the first issue whose title exactly matches."""
    issues = json.load(sys.stdin)
    for issue in issues:
        if issue.get("title") == args.title:
            print(issue["number"])
            break
    return 0


def main() -> int:
    """CLI entrypoint."""
    args = parse_args()
    try:
        if args.command == "render":
            return render_report(args)
        if args.command == "find-open-issue":
            return find_open_issue(args)
    except CommandError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
