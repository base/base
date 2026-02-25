#!/usr/bin/env python3
"""Determine which workspace crates are affected by changes on the current branch.

Outputs package names (one per line) that are either directly changed or
transitively depend on a changed crate. Exits silently with no output
if no workspace crates are affected.

If root Cargo.toml or Cargo.lock changes, outputs all workspace crates.
"""

import argparse
import json
import os
import subprocess
import sys

def get_changed_files(base_branch):
    """Get files changed between base branch and HEAD."""
    result = subprocess.run(
        ["git", "diff", "--name-only", f"{base_branch}...HEAD"],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        # Fallback for when merge-base doesn't exist (e.g., shallow clone)
        result = subprocess.run(
            ["git", "diff", "--name-only", base_branch, "HEAD"],
            capture_output=True, text=True, check=True,
        )
    return [f for f in result.stdout.strip().split("\n") if f]


def get_workspace_metadata():
    """Get workspace metadata (without resolving external deps)."""
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        capture_output=True, text=True, check=True,
    )
    return json.loads(result.stdout)


def build_crate_map(meta):
    """Build mapping from relative crate directory to package name."""
    workspace_root = meta["workspace_root"]
    crate_dirs = {}
    for pkg in meta["packages"]:
        crate_dir = os.path.dirname(pkg["manifest_path"])
        rel_dir = os.path.relpath(crate_dir, workspace_root)
        crate_dirs[rel_dir] = pkg["name"]
    return crate_dirs


def build_reverse_deps(meta):
    """Build reverse dependency graph for workspace members.

    Returns dict: package_name -> set of workspace package names that depend on it.
    """
    workspace_names = {pkg["name"] for pkg in meta["packages"]}
    reverse = {}
    for pkg in meta["packages"]:
        for dep in pkg.get("dependencies", []):
            dep_name = dep.get("rename") or dep["name"]
            if dep_name in workspace_names:
                reverse.setdefault(dep_name, set()).add(pkg["name"])
    return reverse


def transitive_rdeps(changed_crates, reverse_deps):
    """Find all crates transitively affected by the changed crates."""
    affected = set(changed_crates)
    queue = list(changed_crates)
    while queue:
        crate = queue.pop()
        for rdep in reverse_deps.get(crate, set()):
            if rdep not in affected:
                affected.add(rdep)
                queue.append(rdep)
    return affected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", nargs="?", default="main", help="Base branch to diff against")
    parser.add_argument("--exclude", action="append", default=[], help="Package names to exclude")
    args = parser.parse_args()

    changed_files = get_changed_files(args.base)
    if not changed_files:
        return

    meta = get_workspace_metadata()
    all_names = {pkg["name"] for pkg in meta["packages"]}
    exclude = set(args.exclude)

    crate_dirs = build_crate_map(meta)

    # Map changed files to their crate (longest prefix match)
    sorted_dirs = sorted(crate_dirs.keys(), key=len, reverse=True)
    changed_crates = set()
    for f in changed_files:
        for d in sorted_dirs:
            if f.startswith(d + "/") or f == d:
                changed_crates.add(crate_dirs[d])
                break

    if not changed_crates:
        return

    reverse_deps = build_reverse_deps(meta)
    affected = transitive_rdeps(changed_crates, reverse_deps)
    # Only output workspace members, minus exclusions
    affected &= all_names
    affected -= exclude

    for name in sorted(affected):
        print(name)


if __name__ == "__main__":
    main()