#!/usr/bin/env python3
"""Detect whether a change can affect the checked-in SP1 ELF manifest.

The actual ELF binaries are ignored by git. CI uses this script as a cheap
preflight before deciding whether to run the expensive Docker-backed SP1 build
that verifies ``crates/proof/succinct/elf/manifest.toml``.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


MANIFEST = "crates/proof/succinct/elf/manifest.toml"

INPUT_FILES = {
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "etc/just/succinct.just",
}

INPUT_PREFIXES = (
    ".cargo/",
    "crates/common/chains/",
    "crates/common/consensus/",
    "crates/common/evm/",
    "crates/common/flz/",
    "crates/common/genesis/",
    "crates/common/precompile-macros/",
    "crates/common/precompile-storage/",
    "crates/common/precompiles/",
    "crates/common/rpc-types-engine/",
    "crates/consensus/derive/",
    "crates/consensus/protocol/",
    "crates/consensus/upgrades/",
    "crates/proof/driver/",
    "crates/proof/executor/",
    "crates/proof/mpt/",
    "crates/proof/preimage/",
    "crates/proof/primitives/",
    "crates/proof/proof/",
    "crates/proof/succinct/programs/",
    "crates/proof/succinct/utils/build/",
    "crates/proof/succinct/utils/client/",
    "crates/proof/succinct/utils/ethereum/client/",
    "crates/utilities/metrics/",
)


def run_git_diff(args: list[str]) -> list[str] | None:
    """Return changed files for a git diff invocation, or None on failure."""
    result = subprocess.run(
        ["git", "diff", "--name-only", *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    return [line for line in result.stdout.splitlines() if line]


def changed_files(base_ref: str | None) -> list[str]:
    """Return files changed between base_ref and HEAD."""
    if base_ref:
        base_refs = [base_ref]
        if not base_ref.startswith(("origin/", "refs/")):
            base_refs.append(f"origin/{base_ref}")
        for candidate in dict.fromkeys(base_refs):
            for args in ([f"{candidate}...HEAD"], [candidate, "HEAD"]):
                files = run_git_diff(args)
                if files is not None:
                    return files
        raise RuntimeError(f"could not diff against base ref {base_ref}")

    for args in (["HEAD^1...HEAD"], ["HEAD^", "HEAD"]):
        files = run_git_diff(args)
        if files is not None:
            return files
    raise RuntimeError("could not diff HEAD against a parent commit")


def is_elf_input(path: str) -> bool:
    """Return true if path can affect a generated SP1 ELF."""
    return path in INPUT_FILES or any(path.startswith(prefix) for prefix in INPUT_PREFIXES)


def write_output(name: str, value: bool) -> None:
    """Write a GitHub Actions boolean output when running in CI."""
    output_path = os.environ.get("GITHUB_OUTPUT")
    if not output_path:
        return
    with Path(output_path).open("a", encoding="utf-8") as output:
        output.write(f"{name}={str(value).lower()}\n")


def main(argv: list[str]) -> None:
    base_ref = argv[1] if len(argv) > 1 and argv[1] else None
    files = changed_files(base_ref)
    input_changes = [path for path in files if is_elf_input(path)]
    manifest_changed = MANIFEST in files
    needs_rebuild = bool(input_changes or manifest_changed)

    write_output("input_changed", bool(input_changes))
    write_output("manifest_changed", manifest_changed)
    write_output("needs_rebuild", needs_rebuild)

    if not needs_rebuild:
        print("No SP1 ELF inputs changed.")
        return

    if input_changes:
        print("SP1 ELF input changes:")
        for path in input_changes:
            print(f"  {path}")
    if manifest_changed:
        print(f"SP1 ELF manifest changed: {MANIFEST}")


if __name__ == "__main__":
    main(sys.argv)
