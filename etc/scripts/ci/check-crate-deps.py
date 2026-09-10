#!/usr/bin/env python3
"""Validate repository package names, locations and dependency ownership."""

import json
from pathlib import Path
import subprocess
import sys

# These boundaries apply to every dependency, including tests and target-specific edges.
DISALLOWED = {
    "utilities": {"client", "batcher", "builder", "consensus", "execution", "infra", "proof"},
    "client": {"infra"},
    "common": {"client", "batcher", "builder", "consensus", "execution", "infra", "proof", "node"},
    "builder": {"infra", "proof"},
    "consensus": {"infra", "proof"},
    "batcher": {"infra", "proof"},
    "execution": {"infra", "proof"},
    "proof": {"infra"},
}

# Execution subsystems are separate ownership boundaries even though they share
# a top-level domain. Shared RPC/payload schemas under common are not servers.
# Like the domain rules, these restrictions include dev, optional and cfg edges.
EXECUTION_DISALLOWED = {
    "state": {"payload", "rpc", "txpool"},
    "evm": {"engine", "network", "payload", "rpc", "sync", "txpool"},
    "network": {"engine", "payload", "rpc", "sync"},
    "txpool": {"engine", "payload", "sync"},
    "payload": {"engine", "network", "rpc", "sync"},
    "sync": {"payload", "rpc", "txpool"},
    "rpc": {"engine", "sync"},
}

# Forwarding integration tests exercise the RPC server, but the pool itself
# must remain usable without it. Build dependencies are runtime-layer edges too.
EXECUTION_RUNTIME_DISALLOWED = {"txpool": {"rpc"}}

# The state indexer's optional service consumes engine/network events, and
# maintenance tests use the sync pipeline. Neither belongs in foundational
# storage packages; keep their interfaces and implementations below services.
CORE_STATE_PACKAGES = {"types", "database", "mdbx-sys", "provider", "trie"}
CORE_STATE_DISALLOWED = {"engine", "network", "sync"}


def singleton_violations(metadata):
    """Reject grouping directories containing only one library crate."""
    root = Path(metadata["workspace_root"])
    libraries = [
        Path(p["manifest_path"]).relative_to(root).parent
        for p in metadata["packages"]
        if p["id"] in metadata["workspace_members"]
        and Path(p["manifest_path"]).relative_to(root).parts[0] == "crates"
    ]
    errors = []
    for path in libraries:
        # Preserve the domain/subsystem boundary, e.g. crates/execution/txpool.
        # Count all descendants, not just immediate siblings: a group can hold
        # several crates in separate branches without being a singleton.
        candidates = [
            parent for parent in path.parents
            if len(parent.parts) >= 3
            and sum(other.is_relative_to(parent) for other in libraries) == 1
        ]
        if candidates:
            destination = min(candidates, key=lambda p: len(p.parts))
            errors.append(f"{path}: single nested crate must be flattened to {destination}")
    return errors


def violations(metadata):
    """Return all architecture violations in Cargo's workspace metadata."""
    root = Path(metadata["workspace_root"])
    packages = [p for p in metadata["packages"] if p["id"] in metadata["workspace_members"]]
    by_path = {Path(p["manifest_path"]).parent.resolve(): p for p in packages}
    errors = []
    for package in packages:
        name = package["name"]
        relative = Path(package["manifest_path"]).relative_to(root).parent
        parts = relative.parts
        if parts[0] not in ("crates", "bin"):
            errors.append(f"{name}: package must live under crates/ or bin/: {relative}")
        if not name.startswith("base-"):
            errors.append(f"{name}: package name must start with base-")
        if parts[0] == "crates":
            expected = "base-" + "-".join(parts[1:])
            if name != expected:
                errors.append(f"{name}: expected {expected} for {relative}")
        for ancestor in relative.parents:
            if ancestor not in (Path("."), Path("crates"), Path("bin")) and root / ancestor in by_path:
                errors.append(f"{name}: a package cannot be nested inside package {ancestor}")
        source = parts[1] if parts[0] == "crates" else "bin"
        for dependency in package["dependencies"]:
            path = dependency.get("path")
            if not path or Path(path).resolve() not in by_path:
                continue
            target_package = by_path[Path(path).resolve()]
            target_parts = Path(target_package["manifest_path"]).relative_to(root).parent.parts
            target = target_parts[1] if target_parts[0] == "crates" else "bin"
            edge = f"{name} -> {target_package['name']} ({dependency['kind'] or 'normal'})"
            if source == target == "execution":
                source_subsystem = parts[2] if len(parts) > 2 else None
                target_subsystem = target_parts[2] if len(target_parts) > 2 else None
                if target_subsystem in EXECUTION_DISALLOWED.get(source_subsystem, set()):
                    errors.append(
                        f"{edge}: execution/{source_subsystem} cannot depend on "
                        f"execution/{target_subsystem}"
                    )
                if (
                    dependency["kind"] != "dev"
                    and target_subsystem in EXECUTION_RUNTIME_DISALLOWED.get(source_subsystem, set())
                ):
                    errors.append(
                        f"{edge}: execution/{source_subsystem} may depend on "
                        f"execution/{target_subsystem} only in integration tests"
                    )
                if (
                    source_subsystem == "state"
                    and len(parts) > 3
                    and parts[3] in CORE_STATE_PACKAGES
                    and target_subsystem in CORE_STATE_DISALLOWED
                ):
                    errors.append(
                        f"{edge}: foundational state packages cannot depend on "
                        f"execution/{target_subsystem} services"
                    )
            # Shared payloads now include execution outcomes and typed execution errors.
            payload_data = (
                name == "base-common-types-payload"
                and target_package["name"] in {
                    "base-execution-state-types", "base-execution-evm-runtime",
                }
                and dependency["kind"] is None
            )
            if target in DISALLOWED.get(source, set()) and not payload_data:
                errors.append(f"{edge}: {source} cannot depend on {target}")
            if source not in ("bin", "node", "testing") and target in ("node", "bin") and dependency["kind"] != "dev":
                errors.append(f"{edge}: node composition cannot be a lower-layer runtime dependency")
            if source not in ("bin", "testing") and target == "testing" and dependency["kind"] != "dev":
                # Pipeline exports fixtures only behind its explicit test-utils feature.
                pipeline_fixtures = (
                    name == "base-execution-sync"
                    and target_package["name"] == "base-testing-support"
                    and dependency["kind"] is None
                    and dependency["optional"]
                    and "dep:base-testing-support" in package["features"].get("test-utils", [])
                )
                if not pipeline_fixtures:
                    errors.append(f"{edge}: test helpers cannot be a runtime dependency")
    return errors


def main():
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--format-version", "1", "--no-deps", "--locked",
    ]))
    errors = violations(metadata) + singleton_violations(metadata)
    if errors:
        print("\n".join(f"ERROR: {error}" for error in errors))
        print("Rules are defined in etc/scripts/ci/check-crate-deps.py")
        return 1
    print("All crate names, locations and dependencies are valid")
    return 0


if __name__ == "__main__":
    sys.exit(main())
