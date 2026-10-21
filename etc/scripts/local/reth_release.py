"""Build a squashed base/reth tag and pin it from base/base.

`prepare` resolves GitHub PRs, recreates a backport branch from an official
Reth tag, squash-picks each PR, tags the tip, writes the pin manifest, and
rewrites workspace git deps. It does not commit or open a base/base PR.

Bare `--pr N` is a PR on `--upstream-repo`. A full URL may point at that repo
or at `--fork` so Base-specific patches do not have to be opened upstream.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tomllib
import unittest
from dataclasses import dataclass
from pathlib import Path

MANIFEST_REL = Path("etc/upstream-pins/reth.toml")
FORK_CLONE_REL = Path(".tmp/base-reth")
PR_SPEC_RE = re.compile(
    r"^(?:https://github\.com/(?P<owner>[^/]+)/(?P<name>[^/]+)/pull/)?"
    r"(?P<num>\d+)/?$",
    re.IGNORECASE,
)


class ReleaseError(RuntimeError):
    """Raised when preparing a Reth fork pin fails."""


@dataclass(frozen=True)
class PrSpec:
    """A `--pr` value before metadata is fetched.

    `repo` is `owner/repo` from a full pull URL, or `None` when the spec is a
    bare number (resolved against `--upstream-repo`).
    """

    number: int
    repo: str | None


@dataclass(frozen=True)
class PatchPr:
    """One GitHub PR to squash onto the backport branch.

    `repo` is `owner/repo`. That is `paradigmxyz/reth` for upstream backports
    and the Base fork for patches that should not be opened upstream.
    """

    number: int
    repo: str
    url: str
    title: str
    head: str
    commits: tuple[str, ...]


def parse_pr_spec(spec: str) -> PrSpec:
    """Parse a PR number, and the repo when the spec is a GitHub pull URL."""
    match = PR_SPEC_RE.fullmatch(spec.strip())
    if match is None:
        raise ReleaseError(
            f"invalid PR spec {spec!r}; use a number on --upstream-repo "
            "or a GitHub pull URL on that repo or --fork"
        )
    owner = match.group("owner")
    name = match.group("name")
    repo = f"{owner}/{name}" if owner and name else None
    return PrSpec(number=int(match.group("num")), repo=repo)


def resolve_pr_repo(spec: PrSpec, *, upstream_url: str, fork_url: str) -> str:
    """Return `owner/repo` for a PR spec.

    Allowed sources are `--upstream-repo` and `--fork`. Bare numbers use
    `--upstream-repo`.
    """
    upstream = owner_repo(upstream_url)
    fork = owner_repo(fork_url)
    repo = spec.repo or upstream
    allowed = {upstream.lower(), fork.lower()}
    if repo.lower() not in allowed:
        raise ReleaseError(
            f"PR #{spec.number} is on {repo}; patches must come from "
            f"{upstream} or {fork}. Pass a full pull URL on one of those "
            "repos rather than opening a Base-specific change upstream."
        )
    return repo


def squash_marker(pr: PatchPr) -> str:
    """Subject prefix that identifies this PR on the backport branch."""
    return f"backport({pr.repo}#{pr.number}):"


def next_base_tag(upstream_tag: str, existing: list[str]) -> str:
    """Return the next `base-{upstream_tag}.N` tag.

    Names must not match `v*`. `base/reth`'s release workflow builds
    binaries for every `v*` tag.
    """
    prefix = f"base-{upstream_tag}."
    numbers: list[int] = []
    for tag in existing:
        name = tag.removeprefix("refs/tags/")
        if name.startswith(prefix) and name[len(prefix) :].isdigit():
            numbers.append(int(name[len(prefix) :]))
    return f"{prefix}{max(numbers, default=0) + 1}"


def infer_line(root: Path) -> str:
    """Infer the consumer line from the current git branch."""
    branch = git(root, "rev-parse", "--abbrev-ref", "HEAD").strip()
    if branch.startswith("releases/"):
        return branch[len("releases/") :]
    raise ReleaseError(
        f"current branch is {branch}; pass --line (for example v1.3.0)"
    )


def owner_repo(url: str) -> str:
    """Return `owner/repo` from an https GitHub URL."""
    stripped = url.rstrip("/")
    if stripped.endswith(".git"):
        stripped = stripped[: -len(".git")]
    marker = "github.com/"
    idx = stripped.lower().find(marker)
    if idx < 0:
        raise ReleaseError(f"not a GitHub URL: {url}")
    return stripped[idx + len(marker) :]


def run(
    args: list[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    """Run a command and capture text output."""
    result = subprocess.run(
        args,
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if check and result.returncode != 0:
        output = (result.stderr or result.stdout).strip()
        raise ReleaseError(f"`{' '.join(args)}` failed:\n{output}")
    return result


def git(cwd: Path, *args: str, check: bool = True) -> str:
    """Run git in `cwd` and return stdout."""
    return run(["git", *args], cwd=cwd, check=check).stdout


def gh_json(args: list[str]) -> object:
    """Run `gh` and parse JSON stdout."""
    result = run(["gh", *args])
    return json.loads(result.stdout)


def fetch_pr(repo: str, number: int) -> PatchPr:
    """Load PR metadata and the ordered commit list.

    The list is every commit GitHub attributes to the PR, in order. It may
    include merge commits when the author merged the base branch in;
    `squash_pr` drops those before cherry-picking.
    """
    data = gh_json(
        [
            "pr",
            "view",
            str(number),
            "--repo",
            repo,
            "--json",
            "number,url,title,headRefOid,commits",
        ]
    )
    if not isinstance(data, dict):
        raise ReleaseError(f"unexpected gh pr view payload for {repo}#{number}")
    commits: list[str] = []
    for entry in data.get("commits") or []:
        if not isinstance(entry, dict):
            continue
        oid = entry.get("oid")
        if isinstance(oid, str):
            commits.append(oid.lower())
    if not commits:
        raise ReleaseError(f"{repo}#{number} has no commits to squash")
    head = data.get("headRefOid")
    url = data.get("url")
    title = data.get("title")
    if not isinstance(head, str) or not isinstance(url, str) or not isinstance(title, str):
        raise ReleaseError(f"{repo}#{number} is missing url, title, or headRefOid")
    return PatchPr(
        number=int(data.get("number") or number),
        repo=repo,
        url=url,
        title=title,
        head=head.lower(),
        commits=tuple(commits),
    )


def ensure_fork_clone(root: Path, fork_url: str, upstream_url: str) -> Path:
    """Clone or fetch the Reth fork under `.tmp/base-reth`."""
    clone = root / FORK_CLONE_REL
    clone.parent.mkdir(parents=True, exist_ok=True)
    if not (clone / ".git").exists():
        run(["git", "clone", fork_url, str(clone)])
    git(clone, "remote", "set-url", "origin", fork_url)
    remotes = git(clone, "remote").split()
    if "upstream" not in remotes:
        git(clone, "remote", "add", "upstream", upstream_url)
    else:
        git(clone, "remote", "set-url", "upstream", upstream_url)
    git(clone, "fetch", "origin", "--tags", "--prune")
    git(clone, "fetch", "upstream", "--tags", "--prune")
    return clone


def existing_tags(clone: Path) -> list[str]:
    """List tag names from origin and the local clone."""
    tags: set[str] = set()
    remote = run(
        ["git", "ls-remote", "--tags", "origin"],
        cwd=clone,
        check=False,
    )
    for line in remote.stdout.splitlines():
        if not line.strip() or "^{}" in line:
            continue
        tags.add(line.split("\t", 1)[1].removeprefix("refs/tags/"))
    local = git(clone, "tag", "--list")
    tags.update(name for name in local.splitlines() if name)
    return sorted(tags)


def already_squashed(clone: Path, upstream_tag: str, pr: PatchPr) -> bool:
    """Return True when this PR's squash marker is already on the branch."""
    log = git(clone, "log", "--format=%s", f"{upstream_tag}..HEAD", check=False)
    needle = squash_marker(pr)
    return any(line.startswith(needle) for line in log.splitlines())


def non_merge_commits(clone: Path, commits: tuple[str, ...]) -> list[str]:
    """Return the given commits that are not merge commits, preserving order.

    A merge commit (e.g. from merging the base branch into the PR) cannot be
    cherry-picked without `-m`, and its diff is just the base changes we are
    already building on, so it is dropped from the squash.
    """
    kept: list[str] = []
    for commit in commits:
        parents = git(clone, "show", "-s", "--format=%P", commit).split()
        if len(parents) <= 1:
            kept.append(commit)
    return kept


def squash_pr(clone: Path, pr: PatchPr) -> None:
    """Cherry-pick every non-merge PR commit and squash them into one commit."""
    source = f"https://github.com/{pr.repo}"
    local_ref = f"refs/reths/{pr.repo.replace('/', '-')}-{pr.number}"
    git(
        clone,
        "fetch",
        source,
        f"pull/{pr.number}/head:{local_ref}",
        "--force",
    )
    picks = non_merge_commits(clone, pr.commits)
    if not picks:
        raise ReleaseError(
            f"{pr.repo}#{pr.number} has no non-merge commits to squash"
        )
    result = run(
        ["git", "cherry-pick", "-n", *picks],
        cwd=clone,
        check=False,
    )
    if result.returncode != 0:
        run(["git", "cherry-pick", "--abort"], cwd=clone, check=False)
        run(["git", "reset", "--hard"], cwd=clone, check=False)
        raise ReleaseError(
            f"{pr.repo}#{pr.number} does not apply cleanly onto this baseline. "
            "Fix the PR against that baseline, then rerun prepare.\n"
            f"{(result.stderr or result.stdout).strip()}"
        )
    cached = run(["git", "diff", "--cached", "--quiet"], cwd=clone, check=False)
    if cached.returncode == 0:
        raise ReleaseError(
            f"{pr.repo}#{pr.number} produced an empty squash on this baseline"
        )
    message = (
        f"{squash_marker(pr)} {pr.title}\n\n"
        f"Squashed {pr.url} at {pr.head}."
    )
    run(["git", "commit", "-m", message], cwd=clone)


def current_resolved(root: Path) -> list[tuple[str, str]]:
    """Return existing `[[resolved]]` pairs from the pin manifest, if any."""
    path = root / MANIFEST_REL
    if not path.exists():
        return []
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    pairs: list[tuple[str, str]] = []
    for entry in data.get("resolved") or []:
        if not isinstance(entry, dict):
            continue
        pr = entry.get("pr")
        release = entry.get("release")
        if isinstance(pr, str) and isinstance(release, str):
            pairs.append((pr, release))
    return pairs


def render_manifest(
    *,
    repository: str,
    reference: str,
    rev: str,
    upstream_tag: str,
    upstream_rev: str,
    patches: list[PatchPr],
    resolved: list[tuple[str, str]],
) -> str:
    """Render `etc/upstream-pins/reth.toml`."""
    lines = [
        "# Authoritative Reth git pin for [workspace.dependencies].",
        "# See etc/upstream-pins/README.md.",
        "#",
        "# `just pin-reth` rewrites git `reth-*` crates to this repository and",
        "# reference. `just check-reth-pin` verifies Cargo.toml and Cargo.lock.",
        "# `just reth-prepare-release` builds the fork tag and writes this file.",
        "# To return to an official Reth tag, edit this file and run `just pin-reth`.",
        "",
        f'repository = "{repository}"',
        f'reference = "{reference}"',
        f'rev = "{rev}"',
        "",
        "[upstream_base]",
        f'tag = "{upstream_tag}"',
        f'rev = "{upstream_rev}"',
        "",
    ]
    for pr in patches:
        lines.extend(
            [
                "[[patches]]",
                f'pr = "{pr.url}"',
                f'head = "{pr.head}"',
                "",
            ]
        )
    for pr_url, release in resolved:
        lines.extend(
            [
                "[[resolved]]",
                f'pr = "{pr_url}"',
                f'release = "{release}"',
                "",
            ]
        )
    return "\n".join(lines).rstrip() + "\n"


def pin_reth(root: Path, *args: str) -> None:
    """Run the pin helper in this checkout."""
    script = Path(__file__).with_name("pin-reth.py")
    result = run([sys.executable, str(script), *args], cwd=root, check=False)
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    if result.returncode != 0:
        raise ReleaseError("pin-reth failed")


def annotated_tag_message(upstream_tag: str, prs: list[PatchPr]) -> str:
    """Body for the immutable fork tag."""
    lines = [f"Reth {upstream_tag} plus squashed PRs.", ""]
    for pr in prs:
        lines.append(f"- {pr.url} ({pr.head[:12]})")
    return "\n".join(lines)


def prepare_release(
    root: Path,
    *,
    upstream_tag: str,
    pr_specs: list[str],
    line: str | None,
    fork_url: str,
    upstream_url: str,
    skip_push: bool,
) -> None:
    """Build the fork tag, write the pin, and rewrite workspace git deps."""
    if not pr_specs:
        raise ReleaseError("pass at least one --pr")
    line = line or infer_line(root)
    prs: list[PatchPr] = []
    seen_prs: set[tuple[str, int]] = set()
    for spec in (parse_pr_spec(item) for item in pr_specs):
        repo = resolve_pr_repo(spec, upstream_url=upstream_url, fork_url=fork_url)
        identity = (repo.lower(), spec.number)
        if identity in seen_prs:
            raise ReleaseError(f"duplicate --pr: {repo}#{spec.number}")
        seen_prs.add(identity)
        prs.append(fetch_pr(repo, spec.number))
    clone = ensure_fork_clone(root, fork_url, upstream_url)
    git(
        clone,
        "fetch",
        "upstream",
        f"refs/tags/{upstream_tag}:refs/tags/{upstream_tag}",
        "--force",
    )
    upstream_rev = git(clone, "rev-parse", f"{upstream_tag}^{{commit}}").strip().lower()
    fork_tag = next_base_tag(upstream_tag, existing_tags(clone))
    backport_branch = f"backport/{upstream_tag}/{line}"

    remote_backport = run(
        ["git", "ls-remote", "--heads", "origin", backport_branch],
        cwd=clone,
        check=False,
    ).stdout.strip()
    if remote_backport:
        git(clone, "fetch", "origin", backport_branch)
        git(clone, "checkout", "-B", backport_branch, f"origin/{backport_branch}")
    else:
        git(clone, "checkout", "-B", backport_branch, upstream_rev)
    for pr in prs:
        if already_squashed(clone, upstream_tag, pr):
            print(f"already on {backport_branch}: {pr.url}")
            continue
        print(f"squashing {pr.url} ({len(pr.commits)} commits) onto {upstream_tag}")
        squash_pr(clone, pr)
    fork_rev = git(clone, "rev-parse", "HEAD").strip().lower()
    if git(clone, "tag", "--list", fork_tag).strip():
        git(clone, "tag", "-d", fork_tag)
    run(
        ["git", "tag", "-a", fork_tag, "-m", annotated_tag_message(upstream_tag, prs)],
        cwd=clone,
    )

    if not skip_push:
        git(clone, "push", "-u", "origin", backport_branch)
        git(clone, "push", "origin", f"refs/tags/{fork_tag}")

    manifest = render_manifest(
        repository=fork_url.rstrip("/").removesuffix(".git"),
        reference=fork_tag,
        rev=fork_rev,
        upstream_tag=upstream_tag,
        upstream_rev=upstream_rev,
        patches=prs,
        resolved=current_resolved(root),
    )
    (root / MANIFEST_REL).parent.mkdir(parents=True, exist_ok=True)
    (root / MANIFEST_REL).write_text(manifest, encoding="utf-8")
    print(f"wrote {MANIFEST_REL} -> {fork_url} {fork_tag} ({fork_rev})")
    pin_reth(root, "apply")
    print(
        f"prepared {fork_tag} on {backport_branch} "
        f"({len(prs)} PR(s) squashed from {upstream_tag})"
    )


class ReleaseTests(unittest.TestCase):
    """Unit tests for PR specs, tag allocation, and manifest rendering."""

    def test_parse_pr_spec_number_and_url(self) -> None:
        self.assertEqual(parse_pr_spec("26708"), PrSpec(26708, None))
        self.assertEqual(
            parse_pr_spec("https://github.com/paradigmxyz/reth/pull/26766"),
            PrSpec(26766, "paradigmxyz/reth"),
        )
        self.assertEqual(
            parse_pr_spec("https://github.com/base/reth/pull/12"),
            PrSpec(12, "base/reth"),
        )

    def test_parse_pr_spec_rejects_garbage(self) -> None:
        with self.assertRaises(ReleaseError):
            parse_pr_spec("not-a-pr")

    def test_resolve_pr_repo_allows_upstream_and_fork(self) -> None:
        upstream = "https://github.com/paradigmxyz/reth"
        fork = "https://github.com/base/reth"
        self.assertEqual(
            resolve_pr_repo(PrSpec(26708, None), upstream_url=upstream, fork_url=fork),
            "paradigmxyz/reth",
        )
        self.assertEqual(
            resolve_pr_repo(
                PrSpec(12, "base/reth"),
                upstream_url=upstream,
                fork_url=fork,
            ),
            "base/reth",
        )

    def test_resolve_pr_repo_rejects_other_repos(self) -> None:
        with self.assertRaises(ReleaseError):
            resolve_pr_repo(
                PrSpec(1, "someone/reth"),
                upstream_url="https://github.com/paradigmxyz/reth",
                fork_url="https://github.com/base/reth",
            )

    def test_squash_marker_includes_repo(self) -> None:
        upstream = PatchPr(
            number=26708,
            repo="paradigmxyz/reth",
            url="https://github.com/paradigmxyz/reth/pull/26708",
            title="engine handoff",
            head="0b5608325ca86fc2381b49de10b01c975e0ec99f",
            commits=("0b5608325ca86fc2381b49de10b01c975e0ec99f",),
        )
        fork = PatchPr(
            number=26708,
            repo="base/reth",
            url="https://github.com/base/reth/pull/26708",
            title="base-specific",
            head="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            commits=("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",),
        )
        self.assertEqual(squash_marker(upstream), "backport(paradigmxyz/reth#26708):")
        self.assertEqual(squash_marker(fork), "backport(base/reth#26708):")
        self.assertNotEqual(squash_marker(upstream), squash_marker(fork))

    def test_next_base_tag(self) -> None:
        self.assertEqual(next_base_tag("v2.5.1", []), "base-v2.5.1.1")
        self.assertEqual(
            next_base_tag(
                "v2.5.1",
                ["base-v2.5.1.1", "base-v2.5.1.2", "v2.5.1-base.1"],
            ),
            "base-v2.5.1.3",
        )

    def test_render_manifest_records_prs(self) -> None:
        pr = PatchPr(
            number=26708,
            repo="paradigmxyz/reth",
            url="https://github.com/paradigmxyz/reth/pull/26708",
            title="engine handoff",
            head="0b5608325ca86fc2381b49de10b01c975e0ec99f",
            commits=("0b5608325ca86fc2381b49de10b01c975e0ec99f",),
        )
        fork_pr = PatchPr(
            number=12,
            repo="base/reth",
            url="https://github.com/base/reth/pull/12",
            title="base-specific",
            head="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            commits=("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",),
        )
        text = render_manifest(
            repository="https://github.com/base/reth",
            reference="base-v2.5.1.1",
            rev="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            upstream_tag="v2.5.1",
            upstream_rev="6dec1b96b625584956883c34ad0eafbe550480ac",
            patches=[pr, fork_pr],
            resolved=[],
        )
        self.assertIn('repository = "https://github.com/base/reth"', text)
        self.assertIn("etc/upstream-pins/README.md", text)
        self.assertIn(pr.url, text)
        self.assertIn(fork_pr.url, text)
        self.assertNotIn("commit =", text)
