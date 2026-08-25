#!/usr/bin/env python3
"""Apply and verify the workspace Reth git pin.

Reads `etc/upstream-pins/reth.toml` and keeps every git-based `reth-*`
workspace dependency on one repository and one ref. Crates.io Reth crates
(`reth-codecs`, `reth-primitives-traits`, `reth-zstd-compressors`, …) are
left unchanged.

See `etc/upstream-pins/README.md`.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
import unittest
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlparse

MANIFEST_REL = Path("etc/upstream-pins/reth.toml")
CARGO_TOML = Path("Cargo.toml")
CARGO_LOCK = Path("Cargo.lock")
WORKSPACE_DEPS_HEADER = "[workspace.dependencies]"
FULL_SHA_RE = re.compile(r"^[0-9a-fA-F]{40}$")
SHORT_SHA_RE = re.compile(r"^[0-9a-fA-F]{7,40}$")
RETH_CRATE_RE = re.compile(r"^reth(?:-[A-Za-z0-9-]+)?$")
GITHUB_PR_URL_RE = re.compile(
    r"^https://github\.com/[^/]+/[^/]+/pull/\d+$",
    re.IGNORECASE,
)
GIT_DEP_RE = re.compile(
    r"^(?P<name>reth(?:-[A-Za-z0-9-]+)?)\s*=\s*\{\s*"
    r'git\s*=\s*"(?P<git>[^"]+)"\s*,\s*'
    r"(?P<kind>rev|tag)\s*=\s*\"(?P<value>[^\"]+)\""
    r"(?P<rest>.*)$"
)
TOP_LEVEL_REV_RE = re.compile(r'(?m)^rev\s*=\s*"[0-9a-fA-F]*"')
LOCK_SOURCE_RE = re.compile(
    r'^source = "git\+(?P<url>[^"?#]+)(?:\?(?P<query>[^"#]*))?(?:#(?P<sha>[0-9a-fA-F]+))?"\s*$'
)


class PinError(RuntimeError):
    """Raised when the Reth pin is invalid or cannot be applied."""


@dataclass(frozen=True)
class Patch:
    """One GitHub PR squashed onto `upstream_base`.

    `pr` is the full pull URL, including the repository. `head` is the PR tip
    or merge commit that the squash used.
    """

    pr: str
    head: str


@dataclass(frozen=True)
class UpstreamBase:
    """Official Reth tag the pin is based on."""

    tag: str
    rev: str


@dataclass
class Pin:
    """Parsed `etc/upstream-pins/reth.toml`."""

    repository: str
    reference: str
    rev: str
    upstream_base: UpstreamBase
    patches: list[Patch] = field(default_factory=list)
    raw: str = ""


@dataclass(frozen=True)
class GitDep:
    """One git-based `reth-*` workspace dependency line."""

    name: str
    git: str
    kind: str
    value: str
    rest: str


def repo_root_from() -> Path:
    """Return the workspace root that contains the pin manifest."""
    return Path(__file__).resolve().parents[3]


def normalize_git_url(url: str) -> str:
    """Strip trailing slashes and a `.git` suffix for comparison."""
    parsed = urlparse(url.strip())
    path = parsed.path.rstrip("/")
    if path.endswith(".git"):
        path = path[: -len(".git")]
    host = (parsed.netloc or "").lower()
    return f"{parsed.scheme}://{host}{path}"


def is_full_sha(value: str) -> bool:
    """Return True when `value` is a 40-character hex SHA."""
    return bool(FULL_SHA_RE.fullmatch(value))


def cargo_ref(pin: Pin) -> tuple[str, str]:
    """Return the Cargo.toml (`kind`, `value`) pair for this pin."""
    if is_full_sha(pin.reference):
        return "rev", pin.rev
    return "tag", pin.reference


def load_pin(path: Path) -> Pin:
    """Parse the pin manifest from disk."""
    return parse_pin(path.read_text(encoding="utf-8"), path)


def parse_pin(raw: str, path: Path) -> Pin:
    """Parse a pin manifest from a TOML string."""
    data = tomllib.loads(raw)
    repository = data.get("repository")
    reference = data.get("reference")
    rev = data.get("rev")
    if not isinstance(repository, str) or not repository:
        raise PinError(f"{path}: missing string `repository`")
    if not isinstance(reference, str) or not reference:
        raise PinError(f"{path}: missing string `reference`")
    if not isinstance(rev, str) or not is_full_sha(rev):
        raise PinError(f"{path}: `rev` must be a 40-character commit SHA")
    rev = rev.lower()
    if is_full_sha(reference) and reference.lower() != rev:
        raise PinError(f"{path}: `reference` SHA does not match `rev`")

    base_data = data.get("upstream_base")
    if not isinstance(base_data, dict):
        raise PinError(f"{path}: missing `[upstream_base]`")
    base_tag = base_data.get("tag")
    base_rev = base_data.get("rev")
    if not isinstance(base_tag, str) or not base_tag:
        raise PinError(f"{path}: `[upstream_base].tag` is required")
    if not isinstance(base_rev, str) or not is_full_sha(base_rev):
        raise PinError(f"{path}: `[upstream_base].rev` must be a 40-character SHA")

    patches: list[Patch] = []
    seen_prs: set[str] = set()
    for entry in data.get("patches") or []:
        if not isinstance(entry, dict):
            raise PinError(f"{path}: `[[patches]]` entries must be tables")
        if "commit" in entry:
            raise PinError(
                f"{path}: `[[patches]]` records a whole PR. Set `head` to the "
                "PR tip or merge commit; do not use `commit`"
            )
        pr = entry.get("pr")
        head = entry.get("head")
        if not isinstance(pr, str) or not pr:
            raise PinError(f"{path}: `[[patches]].pr` is required")
        pr = pr.rstrip("/")
        if not GITHUB_PR_URL_RE.fullmatch(pr):
            raise PinError(
                f"{path}: `[[patches]].pr` must be a GitHub pull URL so the "
                "repository is part of the identity"
            )
        if pr in seen_prs:
            raise PinError(f"{path}: duplicate `[[patches]]` PR {pr}")
        if not isinstance(head, str) or not SHORT_SHA_RE.fullmatch(head):
            raise PinError(
                f"{path}: `[[patches]].head` must be the PR tip or merge commit SHA"
            )
        seen_prs.add(pr)
        patches.append(Patch(pr=pr, head=head.lower()))

    return Pin(
        repository=repository.rstrip("/"),
        reference=reference,
        rev=rev,
        upstream_base=UpstreamBase(tag=base_tag, rev=base_rev.lower()),
        patches=patches,
        raw=raw,
    )


def set_top_level_rev(raw: str, rev: str) -> str:
    """Replace the top-level `rev` field without touching later tables.

    Only the preamble before the first `[` section header is rewritten, so
    `[upstream_base].rev` cannot be updated by accident.
    """
    match = re.search(r"(?m)^\[", raw)
    preamble = raw if match is None else raw[: match.start()]
    rest = "" if match is None else raw[match.start() :]
    updated, count = TOP_LEVEL_REV_RE.subn(f'rev = "{rev}"', preamble)
    if count != 1:
        raise PinError("could not update top-level `rev` in the pin manifest")
    return updated + rest


def workspace_deps_span(text: str) -> tuple[int, int]:
    """Return the `[workspace.dependencies]` byte range."""
    start = text.find(WORKSPACE_DEPS_HEADER)
    if start < 0:
        raise PinError("Cargo.toml is missing [workspace.dependencies]")
    rest = text[start + len(WORKSPACE_DEPS_HEADER) :]
    nxt = re.search(r"(?m)^\[", rest)
    if nxt is None:
        return start, len(text)
    return start, start + len(WORKSPACE_DEPS_HEADER) + nxt.start()


def parse_git_deps(cargo_toml: str) -> list[GitDep]:
    """Return git-based `reth-*` entries from `[workspace.dependencies]`."""
    start, end = workspace_deps_span(cargo_toml)
    section = cargo_toml[start:end]
    deps: list[GitDep] = []
    for line in section.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        name = stripped.split("=", 1)[0].strip()
        if not RETH_CRATE_RE.fullmatch(name):
            continue
        if "version" in stripped and "git" not in stripped:
            continue
        match = GIT_DEP_RE.match(stripped)
        if match is None:
            raise PinError(
                f"workspace dependency `{name}` looks like a Reth crate but is "
                "not a git `rev`/`tag` pin; crates.io Reth crates must use "
                "`version =` with no `git`"
            )
        deps.append(
            GitDep(
                name=match.group("name"),
                git=match.group("git"),
                kind=match.group("kind"),
                value=match.group("value"),
                rest=match.group("rest"),
            )
        )
    if not deps:
        raise PinError("no git-based `reth-*` workspace dependencies found")
    return deps


def require_uniform_git_deps(deps: list[GitDep]) -> GitDep:
    """Fail when git Reth deps do not all share one repository and ref."""
    urls = {normalize_git_url(dep.git) for dep in deps}
    refs = {(dep.kind, dep.value) for dep in deps}
    if len(urls) != 1 or len(refs) != 1:
        details = ", ".join(f"{dep.name}={dep.kind}:{dep.value}@{dep.git}" for dep in deps)
        raise PinError(f"mixed Reth git pins; all git `reth-*` crates must match: {details}")
    return deps[0]


def rewrite_git_deps(cargo_toml: str, pin: Pin) -> str:
    """Rewrite every git `reth-*` workspace dep to `pin`."""
    deps = parse_git_deps(cargo_toml)
    require_uniform_git_deps(deps)
    kind, value = cargo_ref(pin)
    start, end = workspace_deps_span(cargo_toml)
    section = cargo_toml[start:end]
    lines: list[str] = []
    for line in section.splitlines(keepends=True):
        stripped = line.rstrip("\n")
        newline = "\n" if line.endswith("\n") else ""
        match = GIT_DEP_RE.match(stripped.strip())
        if match is None:
            lines.append(line)
            continue
        rest = match.group("rest")
        new = (
            f'{match.group("name")} = {{ git = "{pin.repository}", '
            f'{kind} = "{value}"{rest}'
        )
        indent = stripped[: len(stripped) - len(stripped.lstrip())]
        lines.append(f"{indent}{new}{newline}")
    return cargo_toml[:start] + "".join(lines) + cargo_toml[end:]


def lockfile_reth_sources(lockfile: str) -> list[tuple[str, str, str]]:
    """Return `(url, query, sha)` for every git source whose path ends in `/reth`."""
    found: list[tuple[str, str, str]] = []
    for line in lockfile.splitlines():
        match = LOCK_SOURCE_RE.match(line)
        if match is None:
            continue
        url = match.group("url")
        if not normalize_git_url(url).endswith("/reth"):
            continue
        found.append((url, match.group("query") or "", (match.group("sha") or "").lower()))
    return found


def verify_lockfile(lockfile: str, pin: Pin) -> None:
    """Require every Reth git lock source to match `pin`."""
    sources = lockfile_reth_sources(lockfile)
    if not sources:
        raise PinError("Cargo.lock has no git sources for a `/reth` repository")
    kind, value = cargo_ref(pin)
    expected_query = f"{kind}={value}"
    expected_url = normalize_git_url(pin.repository)
    for url, query, sha in sources:
        if normalize_git_url(url) != expected_url:
            raise PinError(
                f"Cargo.lock still has Reth git source {url}; expected {pin.repository}"
            )
        if sha != pin.rev:
            raise PinError(
                f"Cargo.lock Reth git SHA is {sha or '<missing>'}; expected {pin.rev}"
            )
        if query != expected_query:
            raise PinError(
                f"Cargo.lock Reth git query is `{query}`; expected `{expected_query}`"
            )


def verify_cargo_toml(deps: list[GitDep], pin: Pin) -> None:
    """Require workspace git Reth deps to match `pin`."""
    sample = require_uniform_git_deps(deps)
    if normalize_git_url(sample.git) != normalize_git_url(pin.repository):
        raise PinError(
            f"Cargo.toml pins {sample.git}; manifest repository is {pin.repository}"
        )
    kind, value = cargo_ref(pin)
    if sample.kind != kind or sample.value != value:
        raise PinError(
            f"Cargo.toml pins {sample.kind} = {sample.value}; "
            f"manifest wants {kind} = {value}"
        )


def commit_from_ls_remote(output: str, reference: str) -> str:
    """Return the commit SHA for `reference` from `git ls-remote` output.

    Annotated tags list the tag object at `refs/tags/<name>` and the commit at
    `refs/tags/<name>^{}`. Prefer the peeled commit.
    """
    peeled: dict[str, str] = {}
    plain: dict[str, str] = {}
    for line in output.splitlines():
        if not line.strip():
            continue
        sha, _, ref = line.partition("\t")
        sha = sha.lower()
        if ref.endswith("^{}"):
            peeled[ref[: -len("^{}")]] = sha
        else:
            plain[ref] = sha
    tag_ref = f"refs/tags/{reference}"
    head_ref = f"refs/heads/{reference}"
    if tag_ref in peeled:
        return peeled[tag_ref]
    if tag_ref in plain:
        return plain[tag_ref]
    if head_ref in plain:
        return plain[head_ref]
    if reference in plain:
        return plain[reference]
    if len(peeled) == 1:
        return next(iter(peeled.values()))
    if len(plain) == 1:
        return next(iter(plain.values()))
    raise PinError(f"could not resolve {reference}")


def resolve_reference(repository: str, reference: str) -> str:
    """Resolve `reference` to a full commit SHA with `git ls-remote`."""
    if is_full_sha(reference):
        return reference.lower()
    result = subprocess.run(
        [
            "git",
            "ls-remote",
            repository,
            f"refs/tags/{reference}",
            f"refs/tags/{reference}^{{}}",
            f"refs/heads/{reference}",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise PinError(
            f"git ls-remote {repository} {reference} failed: {result.stderr.strip()}"
        )
    try:
        return commit_from_ls_remote(result.stdout, reference)
    except PinError as exc:
        raise PinError(f"could not resolve {reference} on {repository}") from exc


def run_cargo(root: Path, args: list[str]) -> None:
    """Run a cargo command at `root` and surface stderr on failure."""
    result = subprocess.run(args, cwd=root, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        output = (result.stderr or result.stdout).strip()
        raise PinError(f"`{' '.join(args)}` failed:\n{output}")


def update_lockfile(root: Path, deps: list[GitDep]) -> None:
    """Refresh Cargo.lock from any git-based Reth workspace crate."""
    names = {dep.name for dep in deps}
    package = "reth-chainspec" if "reth-chainspec" in names else deps[0].name
    run_cargo(root, ["cargo", "update", "-p", package])


def write_if_changed(path: Path, contents: str) -> bool:
    """Write `contents` when they differ from the file on disk."""
    current = path.read_text(encoding="utf-8") if path.exists() else None
    if current == contents:
        return False
    path.write_text(contents, encoding="utf-8")
    return True


def apply_pin(root: Path) -> None:
    """Resolve, rewrite, and refresh the lockfile."""
    manifest_path = root / MANIFEST_REL
    pin = load_pin(manifest_path)
    resolved = resolve_reference(pin.repository, pin.reference)
    if resolved != pin.rev:
        pin.raw = set_top_level_rev(pin.raw, resolved)
        pin.rev = resolved
        write_if_changed(manifest_path, pin.raw)
        print(f"updated {MANIFEST_REL} rev = {resolved}")

    cargo_path = root / CARGO_TOML
    cargo_toml = cargo_path.read_text(encoding="utf-8")
    rewritten = rewrite_git_deps(cargo_toml, pin)
    cargo_changed = write_if_changed(cargo_path, rewritten)
    deps = parse_git_deps(rewritten)
    if cargo_changed:
        print(f"updated {len(deps)} git `reth-*` workspace dependencies")
        update_lockfile(root, deps)
    else:
        print(f"{CARGO_TOML} already matches {MANIFEST_REL}")

    run_cargo(root, ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"])
    verify_lockfile((root / CARGO_LOCK).read_text(encoding="utf-8"), pin)
    print(f"Reth pin is {pin.repository} {cargo_ref(pin)[0]}={cargo_ref(pin)[1]} ({pin.rev})")


def check_pin(root: Path) -> None:
    """Verify the working tree matches the pin manifest."""
    pin = load_pin(root / MANIFEST_REL)
    resolved = resolve_reference(pin.repository, pin.reference)
    if resolved != pin.rev:
        raise PinError(
            f"`reference` {pin.reference} resolves to {resolved}, "
            f"but `rev` is {pin.rev}"
        )
    deps = parse_git_deps((root / CARGO_TOML).read_text(encoding="utf-8"))
    verify_cargo_toml(deps, pin)
    run_cargo(root, ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"])
    verify_lockfile((root / CARGO_LOCK).read_text(encoding="utf-8"), pin)
    print(
        f"Reth pin OK: {len(deps)} git crates -> {pin.repository} "
        f"{cargo_ref(pin)[0]}={cargo_ref(pin)[1]} ({pin.rev})"
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """Parse CLI arguments."""
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command")
    sub.add_parser("apply", help="rewrite workspace git deps from the manifest")
    sub.add_parser("check", help="verify Cargo.toml and Cargo.lock match the manifest")
    sub.add_parser("test", help="run unit tests")
    prepare_p = sub.add_parser(
        "prepare",
        help="squash GitHub PRs onto a Reth tag, publish the fork tag, and pin it",
    )
    prepare_p.add_argument("--upstream", required=True, help="official Reth tag, for example v2.5.1")
    prepare_p.add_argument(
        "--pr",
        action="append",
        required=True,
        help=(
            "PR number on --upstream-repo, or a GitHub pull URL on that repo "
            "or --fork (repeatable)"
        ),
    )
    prepare_p.add_argument("--line", help="consumer line, for example v1.3.0")
    prepare_p.add_argument("--fork", default="https://github.com/base/reth")
    prepare_p.add_argument("--upstream-repo", default="https://github.com/paradigmxyz/reth")
    prepare_p.add_argument("--skip-push", action="store_true")
    args = parser.parse_args(argv)
    if not args.command:
        args.command = "apply"
    return args


def load_release_mod():
    """Load the sibling release helper."""
    script_dir = str(Path(__file__).resolve().parent)
    if script_dir not in sys.path:
        sys.path.insert(0, script_dir)
    import reth_release

    return reth_release


def main(argv: list[str] | None = None) -> int:
    """CLI entrypoint."""
    args = parse_args(argv)
    if args.command == "test":
        return run_tests()
    root = repo_root_from()
    try:
        if args.command == "prepare":
            release = load_release_mod()
            try:
                release.prepare_release(
                    root,
                    upstream_tag=args.upstream,
                    pr_specs=args.pr,
                    line=args.line,
                    fork_url=args.fork,
                    upstream_url=args.upstream_repo,
                    skip_push=args.skip_push,
                )
            except release.ReleaseError as exc:
                print(f"error: {exc}", file=sys.stderr)
                return 1
        elif args.command == "check":
            check_pin(root)
        else:
            apply_pin(root)
    except PinError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


SAMPLE_CARGO = """[workspace.dependencies]
# reth
reth-zstd-compressors = { version = "0.6.0", default-features = false }
reth-db = { git = "https://github.com/paradigmxyz/reth", tag = "v2.5.1" }
reth-cli = { git = "https://github.com/paradigmxyz/reth", tag = "v2.5.1" }
reth-prune-types = { git = "https://github.com/paradigmxyz/reth", tag = "v2.5.1", default-features = false }
"""

SAMPLE_PIN = """repository = "https://github.com/paradigmxyz/reth"
reference = "v2.5.1"
rev = "6dec1b96b625584956883c34ad0eafbe550480ac"

[upstream_base]
tag = "v2.5.1"
rev = "6dec1b96b625584956883c34ad0eafbe550480ac"
"""


def _pin_from(raw: str) -> Pin:
    return parse_pin(raw, Path("reth.toml"))


class PinRethTests(unittest.TestCase):
    """Unit tests for pin parsing, rewrite, and lockfile checks."""

    def test_parse_git_deps_skips_crates_io(self) -> None:
        deps = parse_git_deps(SAMPLE_CARGO)
        self.assertEqual([dep.name for dep in deps], ["reth-db", "reth-cli", "reth-prune-types"])
        self.assertEqual({dep.kind for dep in deps}, {"tag"})

    def test_rewrite_preserves_default_features_and_retargets(self) -> None:
        pin = _pin_from(
            SAMPLE_PIN.replace(
                'repository = "https://github.com/paradigmxyz/reth"',
                'repository = "https://github.com/base/reth"',
            ).replace('reference = "v2.5.1"', 'reference = "v2.5.1-base.1"')
        )
        rewritten = rewrite_git_deps(SAMPLE_CARGO, pin)
        self.assertIn(
            'reth-db = { git = "https://github.com/base/reth", tag = "v2.5.1-base.1" }',
            rewritten,
        )
        self.assertIn(
            'reth-prune-types = { git = "https://github.com/base/reth", tag = "v2.5.1-base.1", default-features = false }',
            rewritten,
        )
        self.assertIn(
            'reth-zstd-compressors = { version = "0.6.0", default-features = false }',
            rewritten,
        )

    def test_rewrite_sha_reference_uses_rev(self) -> None:
        sha = "2fa11e6417e638207aefc11b33028b000a2b1f68"
        pin = _pin_from(
            SAMPLE_PIN.replace(
                'repository = "https://github.com/paradigmxyz/reth"',
                'repository = "https://github.com/niran/reth"',
            )
            .replace('reference = "v2.5.1"', f'reference = "{sha}"')
            .replace("6dec1b96b625584956883c34ad0eafbe550480ac", sha)
        )
        rewritten = rewrite_git_deps(SAMPLE_CARGO, pin)
        self.assertIn(
            f'reth-db = {{ git = "https://github.com/niran/reth", rev = "{sha}" }}',
            rewritten,
        )
        self.assertNotIn('tag = "v2.5.1"', rewritten)

    def test_mixed_pins_are_rejected(self) -> None:
        mixed = SAMPLE_CARGO.replace(
            'reth-cli = { git = "https://github.com/paradigmxyz/reth", tag = "v2.5.1" }',
            'reth-cli = { git = "https://github.com/niran/reth", rev = "2fa11e6417e638207aefc11b33028b000a2b1f68" }',
        )
        with self.assertRaises(PinError):
            require_uniform_git_deps(parse_git_deps(mixed))

    def test_lockfile_query_and_sha(self) -> None:
        lock = (
            'source = "git+https://github.com/paradigmxyz/reth?'
            'tag=v2.5.1#6dec1b96b625584956883c34ad0eafbe550480ac"\n'
        )
        verify_lockfile(lock, _pin_from(SAMPLE_PIN))

    def test_lockfile_rejects_stale_fork(self) -> None:
        lock = (
            'source = "git+https://github.com/niran/reth?'
            'rev=2fa11e6417e638207aefc11b33028b000a2b1f68#'
            '2fa11e6417e638207aefc11b33028b000a2b1f68"\n'
        )
        with self.assertRaises(PinError):
            verify_lockfile(lock, _pin_from(SAMPLE_PIN))

    def test_ls_remote_peels_annotated_tag(self) -> None:
        output = (
            "78b0eec35d6979681423815d49801236149d638f\trefs/tags/v2.5.1-base.1\n"
            "d80b19e5b597f047268c6593300cbbe1f235631b\trefs/tags/v2.5.1-base.1^{}\n"
        )
        self.assertEqual(
            commit_from_ls_remote(output, "v2.5.1-base.1"),
            "d80b19e5b597f047268c6593300cbbe1f235631b",
        )

    def test_ls_remote_lightweight_tag(self) -> None:
        output = "6dec1b96b625584956883c34ad0eafbe550480ac\trefs/tags/v2.5.1\n"
        self.assertEqual(
            commit_from_ls_remote(output, "v2.5.1"),
            "6dec1b96b625584956883c34ad0eafbe550480ac",
        )

    def test_commit_field_is_rejected(self) -> None:
        with self.assertRaises(PinError):
            _pin_from(
                SAMPLE_PIN
                + """
[[patches]]
pr = "https://github.com/paradigmxyz/reth/pull/26708"
commit = "0b5608325ca86fc2381b49de10b01c975e0ec99f"
"""
            )

    def test_duplicate_patch_pr_is_rejected(self) -> None:
        with self.assertRaises(PinError):
            _pin_from(
                SAMPLE_PIN
                + """
[[patches]]
pr = "https://github.com/paradigmxyz/reth/pull/26708"
head = "0b5608325ca86fc2381b49de10b01c975e0ec99f"

[[patches]]
pr = "https://github.com/paradigmxyz/reth/pull/26708"
head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"""
            )

    def test_patch_pr_must_be_github_url(self) -> None:
        with self.assertRaises(PinError):
            _pin_from(
                SAMPLE_PIN
                + """
[[patches]]
pr = "26708"
head = "0b5608325ca86fc2381b49de10b01c975e0ec99f"
"""
            )

    def test_fork_pr_is_valid_patch_identity(self) -> None:
        pin = _pin_from(
            SAMPLE_PIN
            + """
[[patches]]
pr = "https://github.com/base/reth/pull/12"
head = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"""
        )
        self.assertEqual(pin.patches[0].pr, "https://github.com/base/reth/pull/12")

    def test_normalize_git_url(self) -> None:
        self.assertEqual(
            normalize_git_url("https://github.com/paradigmxyz/reth.git/"),
            "https://github.com/paradigmxyz/reth",
        )

    def test_set_top_level_rev_skips_section_rev(self) -> None:
        updated = set_top_level_rev(
            SAMPLE_PIN, "d80b19e5b597f047268c6593300cbbe1f235631b"
        )
        self.assertIn(
            'rev = "d80b19e5b597f047268c6593300cbbe1f235631b"',
            updated.split("[upstream_base]", 1)[0],
        )
        self.assertIn(
            'rev = "6dec1b96b625584956883c34ad0eafbe550480ac"',
            updated.split("[upstream_base]", 1)[1],
        )

    def test_set_top_level_rev_rejects_section_only_rev(self) -> None:
        with self.assertRaises(PinError):
            set_top_level_rev(
                '[upstream_base]\nrev = "6dec1b96b625584956883c34ad0eafbe550480ac"\n',
                "d80b19e5b597f047268c6593300cbbe1f235631b",
            )


def run_tests() -> int:
    """Run colocated unit tests, including the release helper."""
    loader = unittest.defaultTestLoader
    suite = unittest.TestSuite()
    suite.addTests(loader.loadTestsFromTestCase(PinRethTests))
    release = load_release_mod()
    suite.addTests(loader.loadTestsFromTestCase(release.ReleaseTests))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
