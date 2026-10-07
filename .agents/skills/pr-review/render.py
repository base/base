"""Markdown for the comments the review pipeline posts.

Edit this file to change how comments look. The wording of comments is set by the
prompts in agents/; the severity and category vocabulary is defined here and must match
the enums in schemas/ (test_review.py checks that).
"""

from __future__ import annotations

import collections
import dataclasses
import re
from typing import Any

# Every comment this pipeline posts starts with MARKER. The summary keeps the
# marker used by the previous workflow so old summaries are still replaced.
MARKER = "<!-- pr-review -->"
SUMMARY_MARKER = "<!-- CLAUDE_REVIEW_SUMMARY -->"

# Ordered most to least severe.
SEVERITIES = {
    "critical": ("🔴", "Critical"),
    "major": ("🟠", "Major"),
    "minor": ("🟡", "Minor"),
}
# `other` is only a fallback when the decider names no valid category; it is not in the schemas.
CATEGORIES = {
    "block-production": "Block production",
    "correctness": "Correctness",
    "concurrency": "Concurrency",
    "error-handling": "Error handling",
    "safety": "Safety",
    "performance": "Performance",
    "compatibility": "Compatibility",
    "design": "Design",
    "tests": "Tests",
    "other": "Other",
}

# Matches the header line written by Finding.header().
HEADER_RE = re.compile(r"^(?:🔴|🟠|🟡) \*\*(Critical|Major|Minor) · [^*]+\*\* — .+$", re.MULTILINE)
MAX_TITLE_CHARS = 80


def clip(text: str, limit: int) -> str:
    text = text.strip()
    return text if len(text) <= limit else text[: limit - 1].rstrip() + "…"


def severity_rank(severity: str) -> int:
    return list(SEVERITIES).index(severity)


@dataclasses.dataclass(frozen=True)
class Finding:
    """One problem, as it appears in an inline comment or the summary."""

    severity: str
    category: str
    title: str
    body: str
    path: str | None = None
    line: int | None = None
    suggestion: str | None = None
    evidence: str | None = None

    @classmethod
    def from_action(cls, action: dict[str, Any]) -> Finding:
        """Build a finding from a `comment` action, defaulting whatever the decider left out."""
        body = action["body"].strip()
        first_line = next((ln for ln in body.splitlines() if ln.strip()), "")
        return cls(
            severity=action["severity"] if action.get("severity") in SEVERITIES else "minor",
            category=action["category"] if action.get("category") in CATEGORIES else "other",
            title=clip((action.get("title") or "").strip() or first_line, MAX_TITLE_CHARS),
            body=body,
            path=action.get("path"),
            line=action.get("line"),
            suggestion=(action.get("suggestion") or "").strip() or None,
            evidence=(action.get("evidence") or "").strip() or None,
        )

    def header(self) -> str:
        emoji, label = SEVERITIES[self.severity]
        return f"{emoji} **{label} · {CATEGORIES[self.category]}** — {self.title}"

    def markdown(self) -> str:
        parts = [self.header(), self.body]
        if self.suggestion:
            separator = "\n\n" if "\n" in self.suggestion else " "
            parts.append(f"**Suggested fix:**{separator}{self.suggestion}")
        if self.evidence:
            parts.append(f"<details>\n<summary>Evidence</summary>\n\n{self.evidence}\n\n</details>")
        return "\n\n".join(parts)


def thread_header(thread: dict[str, Any]) -> tuple[str | None, str]:
    """The severity (if known) and one-line description of an existing thread's first comment."""
    first = thread["comments"][0]["body"]
    match = HEADER_RE.search(first)
    if match:
        return match.group(1).lower(), match.group(0)
    lines = [ln.strip() for ln in first.replace(MARKER, "").splitlines() if ln.strip()]
    return None, f"💬 {clip(lines[0], 100) if lines else '(empty comment)'}"


def location(repo: str, head_sha: str | None, path: str | None, line: int | None) -> str:
    if not path:
        return ""
    label = f"`{path}:{line}`" if line else f"`{path}`"
    if not head_sha or not line:
        return label
    return f"[{label}](https://github.com/{repo}/blob/{head_sha}/{path}#L{line})"


def headline(counts: collections.Counter) -> str:
    parts = [f"{emoji} {counts[key]} {label.lower()}" for key, (emoji, label) in SEVERITIES.items()
             if counts[key]]
    if counts[None]:
        parts.append(f"💬 {counts[None]} open")
    return " · ".join(parts) or "✅ No open findings"


def cell(text: str) -> str:
    return text.replace("|", "\\|")


def render_summary(*, overview: str | None, new: list[Finding], outside: list[Finding],
                   threads: list[dict[str, Any]], reopened: set[str], failed: list[str],
                   details: str, repo: str, head_sha: str | None,
                   replace_existing: bool) -> str | None:
    """The top-level summary: headline counts, new findings, and what is still open.

    Returns None when there is nothing to report and no earlier summary to replace.
    """
    carried = [t for t in threads if t["owned_by_bot"]
               and (t["thread_id"] in reopened or (not t["resolved"] and not t["outdated"]))]
    if not (new or outside or carried or failed) and not replace_existing:
        return None
    counts: collections.Counter = collections.Counter(f.severity for f in new + outside)
    counts.update(thread_header(t)[0] for t in carried)

    out = [SUMMARY_MARKER, "", f"## {headline(counts)}"]
    if failed:
        names = ", ".join(f"`{name}`" for name in failed)
        out += ["", f"> ⚠️ **Incomplete review:** {names} failed, so some findings may be missing."]
    if overview and overview.strip():
        out += ["", overview.strip()]

    if new:
        out += ["", "### New in this review", "", "| | Category | Finding | Location |", "|---|---|---|---|"]
        for f in new:
            place = location(repo, head_sha, f.path, f.line)
            out.append(f"| {SEVERITIES[f.severity][0]} | {CATEGORIES[f.category]} | {cell(f.title)} | {place} |")

    for heading, is_reopened in (("Reopened", True), ("Open from earlier reviews", False)):
        rows = [t for t in carried if (t["thread_id"] in reopened) == is_reopened]
        if rows:
            out += ["", f"### {heading}", ""]
            for t in rows:
                text = thread_header(t)[1]
                out.append(f"- {text} ([thread]({t['url']}))" if t.get("url") else f"- {text}")

    if outside:
        out += ["", "### Outside the diff", ""]
        for f in outside:
            where = f" (`{f.path}`)" if f.path else ""
            detail = f.markdown().split("\n\n", 1)[1]
            out += [f"- {f.header()}{where}", "", "  " + detail.replace("\n", "\n  "), ""]

    out += ["", "<details>", "<summary>How this was reviewed</summary>", "", details.strip(), "", "</details>"]
    return "\n".join(out) + "\n"


def render_details(*, triage: dict[str, Any], rows: list[tuple[str, str, str]],
                   reported: dict[str, int], dropped: list[dict[str, str]]) -> str:
    """The collapsed 'How this was reviewed' section: depth, models that ran, and what was dropped."""
    sensitive = ", block-production-sensitive" if triage.get("block_production_sensitive") else ""
    out = [f"**Depth:** {triage['depth']}{sensitive}. {triage['reasoning'].strip()}", "",
           "| Stage | Agent | Model |", "|---|---|---|"]
    out += [f"| {stage} | `{name}` | `{model}` |" for stage, name, model in rows]
    if reported:
        out += ["", "**Findings reported to the final step:** "
                + ", ".join(f"`{name}` {count}" for name, count in reported.items())]
    if dropped:
        out += ["", f"**Dropped by the final step ({len(dropped)}):**"]
        out += [f"- {d['title']} — {d['reason']}" for d in dropped]
    return "\n".join(out)
