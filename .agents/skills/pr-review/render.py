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
# The summary records the commit it reviewed, so later pushes can be counted against it. The status block
# is the part that is rewritten on a push, without running the review again.
REVIEWED_RE = re.compile(r"<!-- pr-review:reviewed=([0-9a-f]{7,40}) -->")
STATUS_START = "<!-- pr-review:status -->"
STATUS_END = "<!-- /pr-review:status -->"
RERUN_HELP = ("Reviews run when a pull request is opened or marked ready for review, not on every push. "
              "To review the latest commit, comment `/review` on this pull request; the review then posts a new "
              "summary in place of this one. The pull request's author and the repository's members and "
              "collaborators can do this.")

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
# Matches the plain label the first version of this pipeline put in front of a comment.
LEGACY_RE = re.compile(r"^\*\*(Critical|Major|Minor):\*\*\s*(.+)", re.DOTALL)
# The first line of a bot comment that the bot has marked as resolved (see mark_resolved).
RESOLVED_PREFIX = "✅ **Resolved by the bot**"
ORIGINAL_OPEN = "<details>\n<summary>Original comment</summary>\n\n"
ORIGINAL_CLOSE = "\n\n</details>"
MAX_TITLE_CHARS = 80
# GitHub rejects comment bodies over 65,536 characters; stay well under.
MAX_BODY_CHARS = 30_000
MAX_SUMMARY_FINDING_CHARS = 2_000
MAX_DETAILS_CHARS = 10_000


def clip(text: str, limit: int) -> str:
    text = text.strip()
    return text if len(text) <= limit else text[: limit - 1].rstrip() + "…"


def clip_words(text: str, limit: int) -> str:
    """Clip at a word boundary so the cut does not land inside a code span or word."""
    text = " ".join(text.split())
    if len(text) <= limit:
        return text
    cut = text[: limit - 1]
    cut = cut[: cut.rfind(" ")] if " " in cut else cut
    return cut.rstrip(" ,;:(") + "…"


def is_marked_resolved(body: str) -> bool:
    """Whether a bot comment has been marked as resolved by `mark_resolved`."""
    return body.replace(MARKER, "").lstrip().startswith(RESOLVED_PREFIX)


def mark_resolved(body: str, reason: str) -> str:
    """The bot's own comment rewritten to say that the problem is fixed, keeping the original collapsed.

    GitHub does not let the Actions token resolve a review thread, so the thread stays open on GitHub
    and the comment says what happened instead.
    """
    original = body.replace(MARKER, "").strip()
    return f"{MARKER}\n{RESOLVED_PREFIX} — {clip_words(reason, 300)}\n\n{ORIGINAL_OPEN}{original}{ORIGINAL_CLOSE}"


def unmark_resolved(body: str) -> str | None:
    """The comment as it was before `mark_resolved`, or None if it does not have that shape."""
    start = body.find(ORIGINAL_OPEN)
    end = body.rfind(ORIGINAL_CLOSE)
    if start < 0 or end < start:
        return None
    return f"{MARKER}\n{body[start + len(ORIGINAL_OPEN):end].strip()}"


def reviewed_sha(body: str) -> str | None:
    """The commit a summary comment says it reviewed, or None for a summary from before this was recorded."""
    match = REVIEWED_RE.search(body)
    return match.group(1) if match else None


def status_block(reviewed: str, *, unreviewed: int | None, compare_url: str, files_url: str,
                 uncountable: bool = False) -> str:
    """The line that says how far the pull request has moved past the reviewed commit.

    `unreviewed` is the number of commits pushed since, 0 if none, or None when they cannot be counted:
    because the branch was rewritten, or, with `uncountable`, because the pull request has more commits than
    GitHub will list.
    """
    short = reviewed[:7]
    if uncountable:
        line = (f"⚠️ **This pull request has too many commits to count the ones pushed after `{short}`.** "
                f"[View all changes]({files_url}) · comment `/review` to review them.")
    elif unreviewed == 0:
        line = f"✅ Reviewed `{short}`, the latest commit."
    elif unreviewed is None:
        line = (f"⚠️ **The branch was rewritten after the review of `{short}`**, so the commits that have not "
                f"been reviewed cannot be counted. [View all changes]({files_url}) · comment `/review` to review them.")
    else:
        noun, verb = ("commit", "has") if unreviewed == 1 else ("commits", "have")
        line = (f"⚠️ **{unreviewed} {noun} pushed after `{short}` {verb} not been reviewed.** "
                f"[View the diff]({compare_url}) · comment `/review` to review {'it' if unreviewed == 1 else 'them'}.")
    return f"{STATUS_START}\n> {line}\n{STATUS_END}"


def replace_status(body: str, block: str) -> str | None:
    """`body` with its status block replaced, or None if it has none (an older summary)."""
    start = body.find(STATUS_START)
    end = body.find(STATUS_END, start + 1) if start >= 0 else -1
    if start < 0 or end < 0:
        return None
    return body[:start] + block + body[end + len(STATUS_END):]


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

    def details(self) -> str:
        """Everything but the header: the explanation, the fix, and the evidence."""
        return self.markdown().partition("\n\n")[2]

    def markdown(self) -> str:
        parts = [self.header(), *([self.body] if self.body else [])]
        if self.suggestion:
            separator = "\n\n" if "\n" in self.suggestion else " "
            parts.append(f"**Suggested fix:**{separator}{self.suggestion}")
        if self.evidence:
            parts.append(f"<details>\n<summary>Evidence</summary>\n\n{self.evidence}\n\n</details>")
        return clip("\n\n".join(parts), MAX_BODY_CHARS)


def thread_header(thread: dict[str, Any]) -> tuple[str | None, str]:
    """The severity (if known) and one-line description of an existing thread's first comment."""
    first = thread["comments"][0]["body"]
    if is_marked_resolved(first):
        # Describe the problem, not the note about it: read the original comment kept underneath.
        first = unmark_resolved(first) or first
    first = first.replace(MARKER, "").strip()
    match = HEADER_RE.search(first)
    if match:
        return match.group(1).lower(), match.group(0)
    legacy = LEGACY_RE.match(first)
    if legacy:
        severity = legacy.group(1).lower()
        sentence = re.split(r"(?<=[.!?])\s", legacy.group(2).strip(), maxsplit=1)[0]
        return severity, f"{SEVERITIES[severity][0]} **{legacy.group(1)}** — {clip_words(sentence, 100)}"
    lines = [ln.strip() for ln in first.splitlines() if ln.strip()]
    return None, f"💬 {clip_words(lines[0], 100) if lines else '(empty comment)'}"


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
                   threads: list[dict[str, Any]], reopened: set[str], fixed: set[str], failed: list[str],
                   details: str, repo: str, head_sha: str | None,
                   back: set[str] | frozenset[str] = frozenset(), status: str | None = None) -> str:
    """The top-level summary: the reviewed commit, headline counts, new findings, and what is still open.

    It is always posted, even when nothing was found: it says that the review ran, on which commit, and how
    to run it again.
    """
    # An outdated thread is still open until someone resolves it, so it still counts. A thread the bot
    # marked as resolved in an earlier run is done, unless it was reopened in this one.
    carried = [t for t in threads if t["owned_by_bot"] and t["thread_id"] not in fixed and not t["resolved"]
               and (t["thread_id"] in reopened or t["thread_id"] in back or not t["bot_resolved"])]
    resolved_now = [t for t in threads if t["thread_id"] in fixed]
    counts: collections.Counter = collections.Counter(f.severity for f in new + outside)
    counts.update(thread_header(t)[0] for t in carried)

    out = [SUMMARY_MARKER]
    if head_sha:
        out += [f"<!-- pr-review:reviewed={head_sha} -->", "", f"## {headline(counts)}", "",
                status or status_block(head_sha, unreviewed=0, compare_url="", files_url="")]
    else:
        out += ["", f"## {headline(counts)}"]
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

    groups = (("Reopened", [t for t in carried if t["thread_id"] in reopened]),
              ("Problem is back, but the comment could not be restored",
               [t for t in carried if t["thread_id"] in back and t["thread_id"] not in reopened]),
              ("Open from earlier reviews",
               [t for t in carried if t["thread_id"] not in reopened and t["thread_id"] not in back]))
    for heading, rows in groups:
        if rows:
            out += ["", f"### {heading}", ""]
            for t in rows:
                text = thread_header(t)[1]
                out.append(f"- {text} ([thread]({t['url']}))" if t.get("url") else f"- {text}")

    if resolved_now:
        out += ["", "### Fixed in this push", ""]
        for t in resolved_now:
            link = f" ([thread]({t['url']}))" if t.get("url") else ""
            out.append(f"- ✅ {thread_header(t)[1]}{link}")

    if outside:
        out += ["", "### Outside the diff", ""]
        for f in outside:
            where = f" (`{f.path}`)" if f.path else ""
            detail = clip(f.details(), MAX_SUMMARY_FINDING_CHARS)
            out += [f"- {f.header()}{where}"] + (["", "  " + detail.replace("\n", "\n  "), ""] if detail else [""])

    out += ["", "<details>", "<summary>How this was reviewed</summary>", "", clip(details, MAX_DETAILS_CHARS),
            "", "</details>"]
    # Only a summary on a pull request says how to ask for another review.
    footer = f"\n\n---\n{RERUN_HELP}\n" if head_sha else "\n"
    # Cut the middle if the comment is too long, never the footer: it is how people find out about /review.
    return clip("\n".join(out), MAX_BODY_CHARS - len(footer)) + footer


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
