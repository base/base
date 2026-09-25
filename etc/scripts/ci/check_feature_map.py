#!/usr/bin/env python3
"""Keep the generated coverage inventory in docs/FEATURE_MAP.md in sync with Cargo."""

from __future__ import annotations

import argparse
import sys
import tomllib
from collections import defaultdict
from pathlib import Path

BEGIN = "<!-- BEGIN GENERATED FEATURE MAP INVENTORY -->"
END = "<!-- END GENERATED FEATURE MAP INVENTORY -->"
MAP_PATH = Path("docs/FEATURE_MAP.md")
EXCLUDED_PARTS = {".git", ".agents", "agents", "target"}
REQUIRED_SUPPORT_PATHS = (
    ".config", ".depot", ".github", "actions", "audits", "baseup", "bin", "crates",
    "docs", "etc/benchmarks", "etc/docker", "etc/just", "etc/scripts",
    "etc/systems", "etc/tools", "etc/upstream-pins", "Cargo.toml", "Cargo.lock",
    "Justfile", "README.md", "docker-compose.yml", "rust-toolchain.toml",
)

REQUIRED_HEADINGS = (
    "# Feature Map",
    "## How to use this map",
    "## System paths",
    "## Reth integration boundary",
    "## Product direction and retirement commitments",
    "## Keeping this map current",
)

CATEGORY_RULES = (
    ("actions", "Action harness", "actions/harness/**"),
    ("bin", "Operational binaries", "bin/**"),
    ("crates/batcher", "L1 batching", "crates/batcher/**"),
    ("crates/builder", "Payload building", "crates/builder/**"),
    ("crates/common", "Shared protocol and primitives", "crates/common/**"),
    ("crates/consensus", "Rollup consensus", "crates/consensus/**"),
    ("crates/execution", "Execution node", "crates/execution/**"),
    ("crates/infra", "Operations and E2E tools", "crates/infra/**"),
    ("crates/proof/prover-service", "Prover-service API", "crates/proof/prover-service/**"),
    ("crates/proof/tee", "TEE proving", "crates/proof/tee/**"),
    ("crates/proof/zk", "ZK proving", "crates/proof/zk/**"),
    ("crates/proof", "Proofs and disputes", "crates/proof/**"),
    ("crates/utilities", "Shared utilities", "crates/utilities/**"),
    ("etc/systems", "System-test harness", "etc/systems/**"),
    ("etc/tools", "Developer tools", "etc/tools/**"),
)


def repository_root() -> Path:
    return Path(__file__).resolve().parents[3]


def package_manifests(root: Path) -> list[tuple[str, str]]:
    packages: list[tuple[str, str]] = []
    for manifest in root.rglob("Cargo.toml"):
        relative = manifest.relative_to(root)
        if EXCLUDED_PARTS.intersection(relative.parts):
            continue
        if relative.parts[0] not in {"actions", "bin", "crates", "etc"}:
            continue
        package = tomllib.loads(manifest.read_text()).get("package", {}).get("name")
        if package:
            packages.append((str(relative.parent), package))
    return sorted(packages)


def category_for(path: str) -> tuple[str, str]:
    for prefix, category, pattern in CATEGORY_RULES:
        if path == prefix or path.startswith(f"{prefix}/"):
            return category, pattern
    raise ValueError(f"No feature-map category covers package at {path}")


def reth_dependencies(root: Path) -> tuple[str, list[str]]:
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    dependencies = workspace["workspace"]["dependencies"]
    reth = sorted(name for name in dependencies if name.startswith("reth-"))
    pins = {
        str(spec["tag"])
        for name, spec in dependencies.items()
        if name.startswith("reth-") and isinstance(spec, dict) and "tag" in spec
    }
    pin = ", ".join(sorted(pins)) if pins else "not git-pinned"
    return pin, reth


def grouped_reth_dependencies(dependencies: list[str]) -> list[tuple[str, list[str]]]:
    groups = {
        "Node assembly and CLI": [],
        "Payload, EVM, and execution": [],
        "State, database, and trie": [],
        "RPC": [],
        "P2P and discovery": [],
        "Consensus, primitives, and forks": [],
        "Runtime, tracing, and test support": [],
    }
    for dependency in dependencies:
        if any(token in dependency for token in ("cli", "node")):
            group = "Node assembly and CLI"
        elif any(token in dependency for token in ("payload", "evm", "revm", "execution", "transaction-pool", "exex")):
            group = "Payload, EVM, and execution"
        elif any(token in dependency for token in ("db", "storage", "trie", "provider", "chain-state", "prune", "stages")):
            group = "State, database, and trie"
        elif "rpc" in dependency:
            group = "RPC"
        elif any(token in dependency for token in ("network", "net-", "discv", "ecies", "eth-wire")):
            group = "P2P and discovery"
        elif any(token in dependency for token in ("consensus", "primitives", "chainspec", "forks", "codecs", "zstd")):
            group = "Consensus, primitives, and forks"
        else:
            group = "Runtime, tracing, and test support"
        groups[group].append(dependency)
    return [(group, groups[group]) for group in groups if groups[group]]


def render_inventory(root: Path) -> str:
    grouped_packages: dict[tuple[str, str], list[str]] = defaultdict(list)
    for package_path, package in package_manifests(root):
        grouped_packages[category_for(package_path)].append(package)

    package_lines = []
    for _prefix, category, pattern in CATEGORY_RULES:
        packages = grouped_packages.get((category, pattern), [])
        if packages:
            package_lines.append(f"- **{category}** — `{pattern}` ({len(packages)} packages)")

    pin, dependencies = reth_dependencies(root)
    reth_lines = []
    for group, packages in grouped_reth_dependencies(dependencies):
        reth_lines.append(f"- **{group}** ({len(packages)} direct packages)")

    return "\n".join(
        [
            BEGIN,
            "### Generated repository index",
            "",
            "Generated from Cargo manifests and direct Reth dependencies. It confirms coverage; ",
            "the system-path index above explains ownership and data flow.",
            "",
            "#### Base package groups",
            *package_lines,
            "",
            "#### Direct Reth boundary",
            f"Base Reth git dependencies are pinned at **`{pin}`**.",
            *reth_lines,
            END,
        ]
    )


def replace_inventory(document: str, inventory: str) -> str:
    start = document.find(BEGIN)
    end = document.find(END)
    if start == -1 or end == -1 or end < start:
        raise ValueError("feature map must contain one generated inventory block")
    if document.find(BEGIN, start + 1) != -1 or document.find(END, end + 1) != -1:
        raise ValueError("feature map contains more than one generated inventory block")
    end += len(END)
    return f"{document[:start]}{inventory}{document[end:]}"


def validate_document(root: Path, document: str) -> list[str]:
    errors = []
    for heading in REQUIRED_HEADINGS:
        if heading not in document:
            errors.append(f"missing required heading: {heading}")
    if "RECENT_FEATURE_MAP.md" in document:
        errors.append("feature map still refers to its retired RECENT_FEATURE_MAP.md name")
    if (root / "docs/RECENT_FEATURE_MAP.md").exists():
        errors.append("retired docs/RECENT_FEATURE_MAP.md still exists")
    for path in REQUIRED_SUPPORT_PATHS:
        if not (root / path).exists():
            errors.append(f"documented repository support path no longer exists: {path}")
        elif f"`{path}`" not in document:
            errors.append(f"repository support path is not mapped in the feature map: {path}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="fail when the generated inventory is stale")
    parser.add_argument("--write", action="store_true", help="replace the generated inventory in place")
    args = parser.parse_args()
    if args.check == args.write:
        parser.error("choose exactly one of --check or --write")

    root = repository_root()
    map_path = root / MAP_PATH
    document = map_path.read_text()
    try:
        expected = replace_inventory(document, render_inventory(root))
    except ValueError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1

    errors = validate_document(root, document)
    if args.write:
        map_path.write_text(expected)
        if errors:
            print("\n".join(f"ERROR: {error}" for error in errors), file=sys.stderr)
            return 1
        return 0

    if errors:
        print("\n".join(f"ERROR: {error}" for error in errors), file=sys.stderr)
    if document != expected:
        print(
            "ERROR: docs/FEATURE_MAP.md has a stale generated inventory. "
            "Run `python3 etc/scripts/ci/check_feature_map.py --write`.",
            file=sys.stderr,
        )
    return int(bool(errors or document != expected))


if __name__ == "__main__":
    raise SystemExit(main())
