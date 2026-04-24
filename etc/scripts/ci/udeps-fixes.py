#!/usr/bin/env python3
"""Fix unused Cargo dependencies reported by cargo-udeps.

The workflow is:
1. Run workspace-wide cargo-udeps with JSON output.
2. Remove per-package unused dependencies with `cargo remove`.
3. Re-run cargo-udeps until package-level findings stop changing or a pass limit is hit.
4. Remove root `[workspace.dependencies]` entries that are no longer referenced.

This script is intentionally conservative:
- It only removes package dependencies that cargo-udeps reports directly.
- It only removes workspace dependency definitions when no workspace manifest
  references them with `workspace = true`.
- It expects to run in a clean checkout and leaves git/branch/PR orchestration
  to the caller.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Optional

DEFAULT_MAX_PASSES = 5
UDEPS_ENV = {
    "BASE_SUCCINCT_ELF_STUB": "1",
}
DEPENDENCY_SECTION_NAMES = {"dependencies", "dev-dependencies", "build-dependencies"}


class CommandError(RuntimeError):
    """Raised when a command fails."""


class UdepsFixes:
    """Apply cargo-udeps fixes to the current checkout."""

    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.root = Path.cwd()
        self.root_manifest = self.root / "Cargo.toml"
        self.metadata = self._load_metadata()
        self.manifest_to_package = {
            Path(package["manifest_path"]).resolve(): package["name"]
            for package in self.metadata["packages"]
        }
        self.workspace_manifests = sorted(
            {
                Path(package["manifest_path"]).resolve()
                for package in self.metadata["packages"]
                if Path(package["manifest_path"]).resolve() != self.root_manifest.resolve()
            }
        )

    def run(self) -> int:
        """Run the full fixer flow."""
        self._ensure_clean_tree()

        package_passes = 0
        while package_passes < DEFAULT_MAX_PASSES:
            findings = self._run_udeps()
            removed_any = self._fix_package_findings(findings)
            package_passes += 1
            if not removed_any:
                break
        else:
            print(
                f"Reached max passes ({DEFAULT_MAX_PASSES}) while fixing package dependencies.",
                file=sys.stderr,
            )

        root_removed = self._remove_unused_workspace_dependencies()
        final_findings = self._run_udeps()
        if final_findings:
            print("cargo-udeps still reports unused dependencies after fixes:", file=sys.stderr)
            print(json.dumps(final_findings, indent=2, sort_keys=True), file=sys.stderr)
            if root_removed:
                print(
                    "Removed unused workspace dependencies: "
                    + ", ".join(root_removed),
                    file=sys.stderr,
                )
            if self._worktree_has_changes():
                print("Dependency changes were produced.", file=sys.stderr)
            return 1

        if not self._worktree_has_changes():
            print("No dependency changes were produced.")
            return 0

        if root_removed:
            print(
                "Removed unused workspace dependencies: "
                + ", ".join(root_removed),
            )
        print("Dependency changes were produced.")
        return 0

    def _load_metadata(self) -> dict:
        """Load workspace metadata."""
        return self._run_json(
            [
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--locked",
            ]
        )

    def _ensure_clean_tree(self) -> None:
        """Refuse to run in a dirty checkout."""
        status = self._run(
            ["git", "status", "--short"],
            capture_output=True,
        ).stdout.strip()
        if status:
            raise CommandError("udeps-fixes requires a clean git worktree.")

    def _run_udeps(self) -> dict[str, dict[str, object]]:
        """Run cargo-udeps and return unused dependencies grouped by package."""
        payload = self._run_json(
            [
                "cargo",
                "+nightly",
                "udeps",
                "--locked",
                "--workspace",
                "--all-features",
                "--all-targets",
                "--output",
                "json",
            ],
            env=UDEPS_ENV,
            allow_failure=True,
        )

        unused = payload.get("unused_deps", {})
        findings: dict[str, dict[str, object]] = {}
        for details in unused.values():
            manifest_path = Path(details["manifest_path"]).resolve()
            package = self.manifest_to_package.get(manifest_path)
            if package is None:
                continue
            findings[package] = {
                "manifest_path": manifest_path,
                "normal": list(details.get("normal", [])),
                "development": list(details.get("development", [])),
                "build": list(details.get("build", [])),
            }
        return findings

    def _fix_package_findings(self, findings: dict[str, dict[str, object]]) -> bool:
        """Remove package-level unused dependencies."""
        removed_any = False
        for package, details in sorted(findings.items()):
            normal = sorted(set(details["normal"]))
            development = sorted(set(details["development"]))
            build = sorted(set(details["build"]))
            if normal:
                self._cargo_remove(package, normal)
                removed_any = True
            if development:
                self._cargo_remove(package, development, section="dev")
                removed_any = True
            if build:
                self._cargo_remove(package, build, section="build")
                removed_any = True
        return removed_any

    def _cargo_remove(self, package: str, deps: list[str], section: Optional[str] = None) -> None:
        """Run cargo remove for a package and dependency section."""
        cmd = ["cargo", "remove", "--package", package]
        if section == "dev":
            cmd.append("--dev")
        elif section == "build":
            cmd.append("--build")
        cmd.extend(deps)
        self._run(cmd)

    def _remove_unused_workspace_dependencies(self) -> list[str]:
        """Remove unreferenced root workspace dependencies."""
        workspace_deps = self._read_workspace_dependency_names()
        referenced = self._collect_workspace_dependency_references()
        unused = sorted(workspace_deps - referenced)
        for dep in unused:
            self._remove_workspace_dependency_entry(dep)
        return unused

    def _read_workspace_dependency_names(self) -> set[str]:
        """Return dependency names declared in `[workspace.dependencies]`."""
        manifest = self._load_toml(self.root_manifest)
        workspace = manifest.get("workspace", {})
        if not isinstance(workspace, dict):
            return set()

        dependencies = workspace.get("dependencies", {})
        if not isinstance(dependencies, dict):
            return set()

        return set(dependencies.keys())

    def _collect_workspace_dependency_references(self) -> set[str]:
        """Return dependency names referenced with `workspace = true`."""
        referenced: set[str] = set()
        for manifest_path in self.workspace_manifests:
            referenced |= self._collect_workspace_refs_from_manifest(manifest_path)
        return referenced

    def _collect_workspace_refs_from_manifest(self, manifest_path: Path) -> set[str]:
        """Collect dependency names referenced with `workspace = true` in a manifest."""
        manifest = self._load_toml(manifest_path)
        return self._collect_workspace_refs_from_table(manifest)

    def _collect_workspace_refs_from_table(self, table: object) -> set[str]:
        """Recursively collect dependency names referenced with `workspace = true`."""
        referenced: set[str] = set()
        if not isinstance(table, dict):
            return referenced

        for key, value in table.items():
            if key in DEPENDENCY_SECTION_NAMES and isinstance(value, dict):
                referenced |= self._collect_workspace_refs_from_dependency_section(value)
                continue

            if isinstance(value, dict):
                referenced |= self._collect_workspace_refs_from_table(value)

        return referenced

    def _collect_workspace_refs_from_dependency_section(
        self,
        section: dict[str, object],
    ) -> set[str]:
        """Collect workspace-backed dependencies from a single dependency section."""
        referenced: set[str] = set()
        for dep_name, spec in section.items():
            if isinstance(spec, dict) and spec.get("workspace") is True:
                referenced.add(dep_name)
        return referenced

    def _remove_workspace_dependency_entry(self, dep_name: str) -> None:
        """Remove a dependency entry from `[workspace.dependencies]`."""
        lines = self.root_manifest.read_text().splitlines(keepends=True)
        dep_pattern = re.compile(rf"^{re.escape(dep_name)}\s*=")
        in_workspace_deps = False
        start_index: Optional[int] = None
        end_index: Optional[int] = None
        brace_balance = 0
        bracket_balance = 0

        for index, line in enumerate(lines):
            stripped = line.strip()
            if stripped == "[workspace.dependencies]":
                in_workspace_deps = True
                continue
            if in_workspace_deps and stripped.startswith("[") and stripped != "[workspace.dependencies]":
                break
            if not in_workspace_deps:
                continue

            if start_index is None and dep_pattern.match(line):
                start_index = index
                brace_balance = line.count("{") - line.count("}")
                bracket_balance = line.count("[") - line.count("]")
                if brace_balance <= 0 and bracket_balance <= 0:
                    end_index = index + 1
                    break
                continue

            if start_index is not None:
                brace_balance += line.count("{") - line.count("}")
                bracket_balance += line.count("[") - line.count("]")
                if brace_balance <= 0 and bracket_balance <= 0:
                    end_index = index + 1
                    break

        if start_index is None or end_index is None:
            return

        new_lines = lines[:start_index] + lines[end_index:]
        self.root_manifest.write_text("".join(new_lines))

    def _worktree_has_changes(self) -> bool:
        """Return True when the working tree has changes."""
        result = self._run(
            ["git", "status", "--short"],
            capture_output=True,
        ).stdout.strip()
        return bool(result)

    def _load_toml(self, manifest_path: Path) -> dict:
        """Load a TOML manifest from disk."""
        try:
            return tomllib.loads(manifest_path.read_text())
        except tomllib.TOMLDecodeError as exc:
            raise CommandError(f"Failed to parse TOML: {manifest_path}") from exc

    def _run_json(
        self,
        cmd: list[str],
        env: Optional[dict[str, str]] = None,
        allow_failure: bool = False,
    ) -> dict:
        """Run a command and decode JSON from stdout."""
        result = self._run(cmd, capture_output=True, env=env, check=not allow_failure)
        try:
            return json.loads(result.stdout)
        except json.JSONDecodeError as exc:
            raise CommandError(
                f"Command did not emit valid JSON: {' '.join(cmd)}\n{result.stdout}\n{result.stderr}"
            ) from exc

    def _run(
        self,
        cmd: list[str],
        *,
        capture_output: bool = False,
        env: Optional[dict[str, str]] = None,
        check: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        """Run a subprocess command."""
        merged_env = os.environ.copy()
        if env:
            merged_env.update(env)
        result = subprocess.run(
            cmd,
            capture_output=capture_output,
            check=False,
            cwd=self.root,
            env=merged_env,
            text=True,
        )
        if check and result.returncode != 0:
            raise CommandError(
                f"Command failed ({result.returncode}): {' '.join(cmd)}\n{result.stdout}\n{result.stderr}"
            )
        return result


def parse_args() -> argparse.Namespace:
    """Parse CLI args."""
    parser = argparse.ArgumentParser(description=__doc__)
    return parser.parse_args()


def main() -> int:
    """CLI entrypoint."""
    args = parse_args()
    try:
        return UdepsFixes(args).run()
    except CommandError as exc:
        print(str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
