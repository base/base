"""Build a squashed base/reth tag and pin it from base/base.

`prepare` resolves GitHub PRs, recreates a backport branch from an official
Reth tag, squash-picks each PR, tags the tip, writes the pin manifest, rewrites
workspace git deps, and opens a base/base PR.

PR identity includes the repository. Bare `--pr N` is a PR on `--upstream-repo`.
A full URL may point at that repo or at `--fork` (default `base/reth`) so
Base-specific patches do not have to be opened upstream.

`drop` records upstream PRs in `[[resolved]]` and retargets the pin at an
official Reth release. It refuses while `[[patches]]` still lists fork PRs:
those never land in paradigmxyz/reth.

Pin commits are created from `origin/<base>` (`releases/<line>` or `main`),
not from the local HEAD. See `etc/upstream-pins/README.md`.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tomllib
import unittest
from dataclasses import dataclass
from pathlib import Path

MANIFEST_REL = Path("etc/upstream-pins/reth.toml")
CARGO_TOML = Path("Cargo.toml")
CARGO_LOCK = Path("Cargo.lock")
FORK_CLONE_REL = Path(".tmp/base-reth")
DEFAULT_UPSTREAM_REPO = "https://github.com/paradigmxyz/reth"
DEFAULT_FORK_REPO = "https://github.com/base/reth"
PR_SPEC_RE = re.compile(
    r"^(?:https://github\.com/(?P<owner>[^/]+)/(?P<name>[^/]+)/pull/)?"
    r"(?P<num>\d+)/?$",
    re.IGNORECASE,
)


class ReleaseError(RuntimeError):
    """Raised when preparing or dropping a Reth fork pin fails."""


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
    `--upstream-repo`. Fork PRs are the path for Base-specific patches.
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
    """Return the next `{upstream_tag}-base.N` tag."""
    prefix = f"{upstream_tag}-base."
    numbers: list[int] = []
    for tag in existing:
        name = tag.removeprefix("refs/tags/")
        if name.startswith("refs/tags/"):
            name = name[len("refs/tags/") :]
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
    """Load PR metadata and the ordered non-merge commit list."""
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
    commits_raw = data.get("commits") or []
    commits: list[str] = []
    for entry in commits_raw:
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


def squash_pr(
    clone: Path,
    pr: PatchPr,
    env: dict[str, str],
) -> None:
    """Cherry-pick every PR commit and squash them into one commit."""
    source = f"https://github.com/{pr.repo}"
    local_ref = f"refs/reths/{pr.repo.replace('/', '-')}-{pr.number}"
    git(
        clone,
        "fetch",
        source,
        f"pull/{pr.number}/head:{local_ref}",
        "--force",
    )
    result = run(
        ["git", "cherry-pick", "-n", *pr.commits],
        cwd=clone,
        env=env,
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
    run(["git", "commit", "-m", message], cwd=clone, env=env)


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
        "#",
        "# Workflow and limitations: etc/upstream-pins/README.md",
        "#",
        "# `just pin-reth` rewrites every git-based `reth-*` crate to this repository",
        "# and reference, then refreshes Cargo.lock.",
        "# `just check-reth-pin` verifies Cargo.toml and Cargo.lock match this file.",
        "# `just reth-prepare-release` builds the fork tag and writes this file.",
        "#",
        "# `reference` is the human-facing git ref (release tag, fork tag, or SHA).",
        "# `rev` is the immutable commit that `reference` currently resolves to.",
        "# Cargo.toml uses `tag =` when `reference` is not a SHA, and `rev =` otherwise.",
        "#",
        "# Each [[patches]] entry is a whole GitHub PR squashed to one fork commit.",
        "# Identity is the PR URL, including the repository: paradigmxyz/reth for",
        "# upstream backports, or the Base fork (`base/reth`) for patches that",
        "# must not be opened upstream. `head` is the PR tip (or merge commit).",
        "#",
        "# `[[resolved]]` is only for upstream PRs that landed in an official",
        "# Reth release. Do not record a base/reth PR against an upstream tag.",
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
    if patches:
        for pr in patches:
            lines.extend(
                [
                    "[[patches]]",
                    f'pr = "{pr.url}"',
                    f'head = "{pr.head}"',
                    "",
                ]
            )
    if resolved:
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


def git_identity_env(root: Path) -> dict[str, str]:
    """Copy author identity from the base/base checkout into the fork clone."""
    env = os.environ.copy()
    name = git(root, "log", "-1", "--format=%an").strip()
    email = git(root, "log", "-1", "--format=%ae").strip()
    if name:
        env.setdefault("GIT_AUTHOR_NAME", name)
        env.setdefault("GIT_COMMITTER_NAME", name)
    if email:
        env.setdefault("GIT_AUTHOR_EMAIL", email)
        env.setdefault("GIT_COMMITTER_EMAIL", email)
    return env


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


def prepare_release(
    root: Path,
    *,
    upstream_tag: str,
    pr_specs: list[str],
    line: str | None,
    fork_url: str,
    upstream_url: str,
    dry_run: bool,
    skip_push: bool,
    skip_lock: bool,
    no_commit: bool,
    no_pr: bool,
    base_branch: str | None,
) -> None:
    """Build the fork tag, write the pin, and open the base/base PR."""
    if not pr_specs:
        raise ReleaseError("pass at least one --pr")
    line = line or infer_line(root)
    prs: list[PatchPr] = []
    for spec in (parse_pr_spec(item) for item in pr_specs):
        repo = resolve_pr_repo(spec, upstream_url=upstream_url, fork_url=fork_url)
        prs.append(fetch_pr(repo, spec.number))
    clone = ensure_fork_clone(root, fork_url, upstream_url)
    git(clone, "fetch", "upstream", f"refs/tags/{upstream_tag}:refs/tags/{upstream_tag}", "--force")
    upstream_rev = git(clone, "rev-parse", f"{upstream_tag}^{{commit}}").strip().lower()
    fork_tag = next_base_tag(upstream_tag, existing_tags(clone))
    backport_branch = f"backport/{upstream_tag}/{line}"

    env = git_identity_env(root)
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
        squash_pr(clone, pr, env)
    fork_rev = git(clone, "rev-parse", "HEAD").strip().lower()
    existing_local_tag = git(clone, "tag", "--list", fork_tag).strip()
    if existing_local_tag:
        git(clone, "tag", "-d", fork_tag)
    run(
        ["git", "tag", "-a", fork_tag, "-m", annotated_tag_message(upstream_tag, prs)],
        cwd=clone,
        env=env,
    )

    if not dry_run and not skip_push:
        git(clone, "push", "-u", "origin", backport_branch)
        git(clone, "push", "origin", f"refs/tags/{fork_tag}")

    pin_branch = f"chore/reth-{fork_tag}"
    target_branch = base_branch or default_base_branch(root, line)
    if not no_commit and not dry_run:
        checkout_from_origin(root, branch=pin_branch, base_branch=target_branch)

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

    apply_args = ["apply"]
    if skip_lock:
        apply_args.append("--skip-lock")
    pin_reth(root, *apply_args)

    if not no_commit and not dry_run:
        commit_pin_files(root, fork_tag)
        if not skip_push:
            git(root, "push", "-u", "origin", "HEAD")
            if not no_pr:
                create_base_pr(
                    root,
                    fork_tag=fork_tag,
                    prs=prs,
                    base_branch=target_branch,
                )
    print(
        f"prepared {fork_tag} on {backport_branch} "
        f"({len(prs)} PR(s) squashed from {upstream_tag})"
    )


def annotated_tag_message(upstream_tag: str, prs: list[PatchPr]) -> str:
    """Body for the immutable fork tag."""
    lines = [
        f"Reth {upstream_tag} plus squashed PRs.",
        "",
    ]
    for pr in prs:
        lines.append(f"- {pr.url} ({pr.head[:12]})")
    return "\n".join(lines)


def default_base_branch(root: Path, line: str) -> str:
    """Prefer `releases/<line>` when that branch exists on origin."""
    release_branch = f"releases/{line}"
    result = run(
        ["git", "ls-remote", "--heads", "origin", release_branch],
        cwd=root,
        check=False,
    )
    if result.stdout.strip():
        return release_branch
    return "main"


def checkout_from_origin(root: Path, *, branch: str, base_branch: str) -> None:
    """Create `branch` at the current tip of `origin/{base_branch}`."""
    fetch = run(["git", "fetch", "origin", base_branch], cwd=root, check=False)
    if fetch.returncode != 0:
        raise ReleaseError(
            f"could not fetch origin {base_branch}:\n"
            f"{(fetch.stderr or fetch.stdout).strip()}"
        )
    checkout = run(
        ["git", "checkout", "-B", branch, "FETCH_HEAD"],
        cwd=root,
        check=False,
    )
    if checkout.returncode != 0:
        raise ReleaseError(
            f"could not create {branch} from origin/{base_branch}. "
            "Commit or stash local changes that conflict with that branch, "
            "then rerun.\n"
            f"{(checkout.stderr or checkout.stdout).strip()}"
        )


def commit_pin_files(root: Path, fork_tag: str) -> None:
    """Commit the pin manifest and rewritten Cargo files on the current branch."""
    git(root, "add", str(MANIFEST_REL), str(CARGO_TOML), str(CARGO_LOCK))
    cached = run(["git", "diff", "--cached", "--quiet"], cwd=root, check=False)
    if cached.returncode == 0:
        print("no pin file changes to commit")
        return
    git(
        root,
        "commit",
        "-m",
        f"chore(deps): pin reth {fork_tag}",
    )


def create_base_pr(
    root: Path,
    *,
    fork_tag: str,
    prs: list[PatchPr],
    base_branch: str,
) -> None:
    """Open a base/base PR for the generated pin."""
    rows = "\n".join(f"| {pr.url} | `{pr.head}` |" for pr in prs)
    body = f"""## Summary

Pin Reth git workspace deps to `{fork_tag}`.

| PR | Head |
| --- | --- |
{rows}

## Test plan

- [x] `just pin-reth-test`
- [x] `just check-reth-pin`
- [ ] `cargo check --locked -p base-reth-node -p base-builder-bin -p base-consensus`
"""
    run(
        [
            "gh",
            "pr",
            "create",
            "--base",
            base_branch,
            "--title",
            f"chore(deps): pin reth {fork_tag}",
            "--body",
            body,
        ],
        cwd=root,
    )


def fork_only_patch_urls(patches: object, *, upstream_url: str) -> list[str]:
    """Return PR URLs that are not on `--upstream-repo`.

    `drop` cannot claim those landed in an official Reth release. Retiring
    Base-specific fork PRs is a later step; until then they stay in
    `[[patches]]`.
    """
    if not isinstance(patches, list):
        return []
    upstream = owner_repo(upstream_url).lower()
    leftover: list[str] = []
    for entry in patches:
        if not isinstance(entry, dict) or not isinstance(entry.get("pr"), str):
            continue
        spec = parse_pr_spec(entry["pr"])
        repo = (spec.repo or upstream).lower()
        if repo != upstream:
            leftover.append(entry["pr"])
    return leftover


def drop_pin(
    root: Path,
    *,
    release: str,
    upstream_url: str,
    skip_lock: bool,
    dry_run: bool,
    no_commit: bool,
    no_pr: bool,
    skip_push: bool,
    base_branch: str | None,
) -> None:
    """Retarget the pin at an official release that contains the carried PRs."""
    path = root / MANIFEST_REL
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    patches = data.get("patches") or []
    if not patches:
        raise ReleaseError("no [[patches]] to resolve")
    leftover = fork_only_patch_urls(patches, upstream_url=upstream_url)
    if leftover:
        details = ", ".join(leftover)
        raise ReleaseError(
            "cannot drop the fork pin while carrying Base-specific PRs "
            f"({details}). Those stay on the fork until they are replaced; "
            "`drop` only records official Reth releases for "
            f"{owner_repo(upstream_url)} PRs."
        )
    resolved = list(data.get("resolved") or [])
    for entry in patches:
        if not isinstance(entry, dict) or not isinstance(entry.get("pr"), str):
            raise ReleaseError("each [[patches]] entry needs `pr`")
        resolved.append({"pr": entry["pr"], "release": release})
    dropped_prs: list[PatchPr] = []
    for entry in patches:
        if not isinstance(entry, dict) or not isinstance(entry.get("pr"), str):
            continue
        spec = parse_pr_spec(entry["pr"])
        dropped_prs.append(
            PatchPr(
                number=spec.number,
                repo=spec.repo or owner_repo(upstream_url),
                url=entry["pr"],
                title="",
                head=str(entry.get("head") or "0" * 40),
                commits=(),
            )
        )
    clone = ensure_fork_clone(root, DEFAULT_FORK_REPO, upstream_url)
    git(clone, "fetch", "upstream", f"refs/tags/{release}:refs/tags/{release}", "--force")
    upstream_rev = git(clone, "rev-parse", f"{release}^{{commit}}").strip().lower()
    resolved_pairs = [(str(item["pr"]), str(item["release"])) for item in resolved]
    manifest = render_manifest(
        repository=upstream_url.rstrip("/").removesuffix(".git"),
        reference=release,
        rev=upstream_rev,
        upstream_tag=release,
        upstream_rev=upstream_rev,
        patches=[],
        resolved=resolved_pairs,
    )
    pin_branch = f"chore/reth-{release}"
    target_branch = base_branch or "main"
    if not no_commit and not dry_run:
        checkout_from_origin(root, branch=pin_branch, base_branch=target_branch)

    path.write_text(manifest, encoding="utf-8")
    apply_args = ["apply"]
    if skip_lock:
        apply_args.append("--skip-lock")
    pin_reth(root, *apply_args)
    if not no_commit and not dry_run:
        commit_pin_files(root, release)
        if not skip_push:
            git(root, "push", "-u", "origin", "HEAD")
            if not no_pr:
                create_base_pr(
                    root,
                    fork_tag=release,
                    prs=dropped_prs,
                    base_branch=target_branch,
                )
    print(f"dropped fork pin; now tracking {upstream_url} {release} ({upstream_rev})")


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

    def test_drop_refuses_fork_only_prs(self) -> None:
        leftover = fork_only_patch_urls(
            [
                {
                    "pr": "https://github.com/paradigmxyz/reth/pull/26708",
                    "head": "0b5608325ca86fc2381b49de10b01c975e0ec99f",
                },
                {
                    "pr": "https://github.com/base/reth/pull/12",
                    "head": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                },
            ],
            upstream_url="https://github.com/paradigmxyz/reth",
        )
        self.assertEqual(leftover, ["https://github.com/base/reth/pull/12"])
        self.assertEqual(
            fork_only_patch_urls(
                [
                    {
                        "pr": "https://github.com/paradigmxyz/reth/pull/26708",
                        "head": "0b5608325ca86fc2381b49de10b01c975e0ec99f",
                    }
                ],
                upstream_url="https://github.com/paradigmxyz/reth",
            ),
            [],
        )

    def test_next_base_tag(self) -> None:
        self.assertEqual(next_base_tag("v2.5.1", []), "v2.5.1-base.1")
        self.assertEqual(
            next_base_tag("v2.5.1", ["v2.5.1-base.1", "v2.5.1-base.2", "v2.4.0-base.9"]),
            "v2.5.1-base.3",
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
            reference="v2.5.1-base.1",
            rev="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            upstream_tag="v2.5.1",
            upstream_rev="6dec1b96b625584956883c34ad0eafbe550480ac",
            patches=[pr, fork_pr],
            resolved=[],
        )
        self.assertIn('repository = "https://github.com/base/reth"', text)
        self.assertIn("etc/upstream-pins/README.md", text)
        self.assertIn('reference = "v2.5.1-base.1"', text)
        self.assertIn("[[patches]]", text)
        self.assertIn(pr.url, text)
        self.assertIn(fork_pr.url, text)
        self.assertIn(pr.head, text)
        self.assertNotIn("commit =", text)
        self.assertIn("must not be opened upstream", text)
