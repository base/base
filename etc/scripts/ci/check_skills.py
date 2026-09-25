#!/usr/bin/env python3
"""Validate the repository's curated skills."""

from __future__ import annotations

import argparse
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
SKILLS_DIR = ROOT / ".agents/skills"
EXPECTED_SKILLS = {
    "agent-instruction-auditor",
    "derivation-and-l1-following",
    "performance-evidence",
    "system-validation-and-devnets",
}
REQUIRED_SKILL_HEADINGS = {
    "derivation-and-l1-following": (
        "## Start here",
        "## Flow and ownership",
        "## Invariants and failure modes",
        "## Validation",
    ),
    "performance-evidence": (
        "## Choose the evidence tier",
        "## Comparison contract",
    ),
    "system-validation-and-devnets": (
        "## Start here",
        "## Choose the smallest evidence level that proves the contract",
        "## Failure classification",
        "## Validation report",
    ),
}
LINK = re.compile(r"(?<!!)\[[^]]+\]\(([^)]+)\)")
NAME = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*\Z")


def fail(errors: list[str], message: str) -> None:
    errors.append(message)


def parse_frontmatter(path: Path, errors: list[str]) -> tuple[dict[str, str], str]:
    text = path.read_text()
    if not text.startswith("---\n"):
        fail(errors, f"{path.relative_to(ROOT)}: missing opening frontmatter delimiter")
        return {}, text
    try:
        _opening, frontmatter, body = text.split("---\n", 2)
    except ValueError:
        fail(errors, f"{path.relative_to(ROOT)}: missing closing frontmatter delimiter")
        return {}, text

    fields: dict[str, str] = {}
    for line in frontmatter.strip().splitlines():
        if ": " not in line:
            fail(errors, f"{path.relative_to(ROOT)}: invalid frontmatter line: {line!r}")
            continue
        key, value = line.split(": ", 1)
        if key in fields:
            fail(errors, f"{path.relative_to(ROOT)}: duplicate frontmatter key: {key}")
        fields[key] = value
    return fields, body


def check_links(path: Path, text: str, errors: list[str]) -> None:
    for target in LINK.findall(text):
        target = target.split("#", 1)[0]
        if not target or "://" in target or target.startswith(("mailto:", "/")):
            continue
        if not (path.parent / target).resolve().exists():
            fail(errors, f"{path.relative_to(ROOT)}: broken local link: {target}")


def check_skills(errors: list[str]) -> None:
    found = {path.parent.name for path in SKILLS_DIR.glob("*/SKILL.md")}
    missing = EXPECTED_SKILLS - found
    unexpected = found - EXPECTED_SKILLS
    if missing:
        fail(errors, f"missing required skills: {', '.join(sorted(missing))}")
    if unexpected:
        fail(errors, f"unregistered skills: {', '.join(sorted(unexpected))}")

    for path in sorted(SKILLS_DIR.glob("*/SKILL.md")):
        fields, body = parse_frontmatter(path, errors)
        relative = path.relative_to(ROOT)
        name = fields.get("name", "")
        if name != path.parent.name:
            fail(errors, f"{relative}: frontmatter name must match directory name")
        if not NAME.fullmatch(name):
            fail(errors, f"{relative}: invalid skill name")
        if not fields.get("description", "").strip():
            fail(errors, f"{relative}: description must not be empty")
        if not body.lstrip().startswith("# "):
            fail(errors, f"{relative}: body must begin with a level-one heading")
        check_links(path, body, errors)
        for heading in REQUIRED_SKILL_HEADINGS.get(name, ()):
            if heading not in body:
                fail(errors, f"{relative}: missing required section {heading!r}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="validate curated skill structure")
    args = parser.parse_args()
    if not args.check:
        parser.error("choose --check")

    errors: list[str] = []
    check_skills(errors)
    if errors:
        print("\n".join(f"ERROR: {error}" for error in errors))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
