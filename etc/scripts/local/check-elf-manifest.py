#!/usr/bin/env python3
"""Verify or refresh the sha256 entries in ``crates/proof/succinct/elf/manifest.toml``.

Usage::

    check_manifest.py <manifest.toml> <cache_dir>                 # verify, print status
    check_manifest.py --write <manifest.toml> <cache_dir>          # rewrite sha256 fields
    check_manifest.py --print-hashes <manifest.toml> <cache_dir>   # print expected/actual hashes

Verify mode exit code is always 0; the resulting status is printed to stdout as
one of ``match``, ``missing:<name>``, or ``mismatch:<name>`` so the just recipe
can branch on it.

Uses only the Python standard library so it runs on a bare CI runner.
"""

from __future__ import annotations

import hashlib
import re
import sys
from pathlib import Path


def parse_manifest(text: str) -> list[tuple[str, str]]:
    """Return a list of ``(name, sha256)`` pairs from the manifest."""
    entries: list[tuple[str, str]] = []
    name: str | None = None
    sha: str | None = None
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("[[elfs]]"):
            if name is not None and sha is not None:
                entries.append((name, sha))
            name, sha = None, None
        elif match := re.match(r'name\s*=\s*"([^"]+)"', line):
            name = match.group(1)
        elif match := re.match(r'sha256\s*=\s*"([^"]+)"', line):
            sha = match.group(1)
    if name is not None and sha is not None:
        entries.append((name, sha))
    return entries


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify(manifest_path: Path, cache_dir: Path) -> str:
    entries = parse_manifest(manifest_path.read_text())
    if not entries:
        return "empty-manifest"
    for name, expected in entries:
        target = cache_dir / name
        if not target.exists():
            return f"missing:{name}"
        if sha256_of(target) != expected:
            return f"mismatch:{name}"
    return "match"


def write(manifest_path: Path, cache_dir: Path) -> None:
    text = manifest_path.read_text()
    entries = parse_manifest(text)
    for name, _ in entries:
        target = cache_dir / name
        if not target.exists():
            print(f"error: {target} not present; cannot update manifest", file=sys.stderr)
            sys.exit(1)
        actual = sha256_of(target)
        # Replace the sha256 line immediately following the matching name entry.
        pattern = re.compile(
            r'(name\s*=\s*"' + re.escape(name) + r'"\s*\nsha256\s*=\s*)"[^"]*"',
            re.MULTILINE,
        )
        new_text, count = pattern.subn(rf'\1"{actual}"', text, count=1)
        if count != 1:
            print(f"error: could not locate sha256 entry for {name}", file=sys.stderr)
            sys.exit(1)
        text = new_text
    manifest_path.write_text(text)


def print_hashes(manifest_path: Path, cache_dir: Path) -> None:
    """Print the manifest hash and current cache hash for each ELF."""
    entries = parse_manifest(manifest_path.read_text())
    if not entries:
        print("empty-manifest")
        return
    for name, expected in entries:
        target = cache_dir / name
        actual = sha256_of(target) if target.exists() else "<missing>"
        print(f'{name}: expected="{expected}" actual="{actual}"')


def main(argv: list[str]) -> None:
    args = argv[1:]
    write_mode = False
    print_hashes_mode = False
    while args and args[0].startswith("--"):
        flag = args[0]
        if flag == "--write":
            write_mode = True
        elif flag == "--print-hashes":
            print_hashes_mode = True
        else:
            print(f"unknown option: {flag}", file=sys.stderr)
            sys.exit(2)
        args = args[1:]
    if len(args) != 2:
        print(
            "usage: check_manifest.py [--write] [--print-hashes] <manifest.toml> <cache_dir>",
            file=sys.stderr,
        )
        sys.exit(2)
    manifest_path = Path(args[0])
    cache_dir = Path(args[1])
    if write_mode:
        write(manifest_path, cache_dir)
        if print_hashes_mode:
            print_hashes(manifest_path, cache_dir)
        return
    if print_hashes_mode:
        print_hashes(manifest_path, cache_dir)
    else:
        print(verify(manifest_path, cache_dir))


if __name__ == "__main__":
    main(sys.argv)
