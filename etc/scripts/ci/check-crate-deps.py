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
            target_parts = Path(target_package["manifest_path"]).relative_to(root).parts
            target = target_parts[1] if target_parts[0] == "crates" else "bin"
            edge = f"{name} -> {target_package['name']} ({dependency['kind'] or 'normal'})"
            if target in DISALLOWED.get(source, set()):
                errors.append(f"{edge}: {source} cannot depend on {target}")
            if source not in ("bin", "node", "testing") and target in ("node", "bin") and dependency["kind"] != "dev":
                errors.append(f"{edge}: node composition cannot be a lower-layer runtime dependency")
            if source not in ("bin", "testing") and target == "testing" and dependency["kind"] != "dev":
                # Node's debug source is an operational tool, not a fixture dependency.
                debug_source = name == "base-node-service" and target_package["name"] == "base-testing-debug-client" and dependency["kind"] is None
                # Pipeline exports fixtures only behind its explicit test-utils feature.
                pipeline_fixtures = (
                    name == "base-execution-sync-pipeline"
                    and target_package["name"] == "base-testing-support"
                    and dependency["kind"] is None
                    and dependency["optional"]
                    and "dep:base-testing-support" in package["features"].get("test-utils", [])
                )
                if not (debug_source or pipeline_fixtures):
                    errors.append(f"{edge}: test helpers cannot be a runtime dependency")
    return errors


def main():
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--format-version", "1", "--no-deps", "--locked",
    ]))
    errors = violations(metadata)
    if errors:
        print("\n".join(f"ERROR: {error}" for error in errors))
        print("Rules are defined in etc/scripts/ci/check-crate-deps.py")
        return 1
    print("All crate names, locations and dependencies are valid")
    return 0


if __name__ == "__main__":
    sys.exit(main())
