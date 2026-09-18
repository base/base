#!/usr/bin/env python3
"""Record the source, commands, and binary/image identities used by an acceptance run.

Snapshot before builds, capture after builds, then verify immediately before execution.
This detects changes during preparation; it is not a hermetic build attestation.
Dirty patches/logs may contain sensitive data. Review artifacts before sharing.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


def command(*args: str, cwd: Path | None = None) -> str:
    """Run a bounded inspection command, preserving errors for the caller."""
    return subprocess.run(
        args, cwd=cwd, check=True, capture_output=True, text=True, timeout=30
    ).stdout.removesuffix("\n")


def source_state(root: Path) -> tuple[dict, bytes]:
    """Identify tracked changes and untracked source without recording the environment."""
    status = command("git", "status", "--porcelain=v1", "--untracked-files=all", cwd=root)
    patch = subprocess.run(
        ["git", "diff", "--binary", "HEAD"], cwd=root, check=True,
        capture_output=True, timeout=30,
    ).stdout
    untracked = command("git", "ls-files", "--others", "--exclude-standard", "-z", cwd=root)
    untracked_hashes = {}
    for name in filter(None, untracked.split("\0")):
        path = root / name
        contents = str(path.readlink()).encode() if path.is_symlink() else path.read_bytes()
        untracked_hashes[name] = hashlib.sha256(contents).hexdigest()
    return {
        "workspace_root": str(root),
        "revision": command("git", "rev-parse", "HEAD", cwd=root),
        "dirty": bool(status),
        "status": status,
        "patch_sha256": hashlib.sha256(patch).hexdigest(),
        "untracked_sha256": untracked_hashes,
    }, patch


def binary_identities(path: Path) -> list[dict]:
    """Hash the compiled acceptance binary selected by nextest, not a guessed target path."""
    metadata = json.loads(path.read_text())
    binaries = metadata["rust-binaries"]
    if set(binaries) != {"base-system-tests::acceptance"}:
        raise ValueError(f"expected only the acceptance binary, got {list(binaries)}")
    identities = []
    for binary_id, binary in binaries.items():
        binary_path = Path(binary["binary-path"])
        with binary_path.open("rb") as contents:
            digest = hashlib.file_digest(contents, "sha256").hexdigest()
        identities.append({
            "binary_id": binary_id, "path": str(binary_path),
            "sha256": digest, "size": binary_path.stat().st_size,
        })
    return identities


def snapshot(directory: Path, root: Path) -> None:
    """Capture source before any preparation, refusing existing provenance."""
    source, _ = source_state(root)
    with (directory / "source-before-build.json").open("x") as output:
        json.dump(source, output, indent=2)
        output.write("\n")


def capture(directory: Path, root: Path, images: list[str], build_commands: list[str]) -> None:
    """Bind unchanged build-time source to actual compiled binaries and Docker images."""
    source, patch = source_state(root)
    if source != json.loads((directory / "source-before-build.json").read_text()):
        raise ValueError("source changed during preparation; use a fresh run")
    identities = []
    for image in images:
        inspected = json.loads(command("docker", "image", "inspect", image))[0]
        labels = inspected.get("Config", {}).get("Labels") or {}
        identities.append({
            "reference": image,
            "id": inspected["Id"],
            "repo_digests": inspected.get("RepoDigests", []),
            "architecture": inspected["Architecture"],
            "os": inspected["Os"],
            "labels": {
                key: labels[key]
                for key in (
                    "org.opencontainers.image.revision",
                    "org.opencontainers.image.source",
                    "org.opencontainers.image.version",
                    "org.base.devnet.setup.validator-count",
                )
                if key in labels
            },
        })
    pins = json.loads((directory / "client-pins.json").read_text())
    versions = {}
    for client in ("reth", "lighthouse"):
        reference = pins[client]["image"]
        if reference not in images:
            raise ValueError(f"missing actual image identity for {client}")
        versions[client] = command(
            "docker", "run", "--rm", "--network", "none", "--entrypoint", client,
            reference, "--version",
        )
        if not versions[client]:
            raise ValueError(f"empty {client} --version output")
    provenance = {
        "schema_version": 1,
        "source": source,
        "client_versions": versions,
        "build_commands": build_commands,
        "binaries": binary_identities(directory / "nextest-binaries.json"),
        "tools": {
            "rustc": command("rustc", "--version", "--verbose"),
            "cargo": command("cargo", "--version"),
            "nextest": command("cargo", "nextest", "--version"),
            "docker": command("docker", "version", "--format", "{{json .}}"),
        },
        "images": identities,
    }
    with (directory / "provenance.json").open("x") as output:
        json.dump(provenance, output, indent=2)
        output.write("\n")
    with (directory / "source.patch").open("xb") as output:
        output.write(patch)


def verify(directory: Path, root: Path) -> None:
    """Fail if source or the previously built binary changed before execution."""
    provenance = json.loads((directory / "provenance.json").read_text())
    if source_state(root)[0] != provenance["source"]:
        raise ValueError("source changed after build; use a fresh run")
    if binary_identities(directory / "nextest-binaries.json") != provenance["binaries"]:
        raise ValueError("acceptance binary changed after build; use a fresh run")
    for image in provenance["images"]:
        actual = json.loads(command("docker", "image", "inspect", image["reference"]))[0]
        if actual["Id"] != image["id"]:
            raise ValueError(f"image changed after preparation: {image['reference']}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("snapshot", "capture", "verify"))
    parser.add_argument("directory", type=Path)
    parser.add_argument("--image", action="append", default=[])
    parser.add_argument("--build-command", action="append", default=[])
    args = parser.parse_args()
    root = Path(command("git", "rev-parse", "--show-toplevel")).resolve()
    if args.action == "snapshot":
        snapshot(args.directory, root)
    elif args.action == "verify":
        verify(args.directory, root)
    else:
        if not args.image or not args.build_command:
            parser.error("capture requires --image and --build-command")
        capture(args.directory, root, args.image, args.build_command)


class ProvenanceTests(unittest.TestCase):
    """Exercise source/binary mutation detection against owned temporary files and Git state."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.root = self.directory / "source"
        self.root.mkdir()
        command("git", "init", "-q", cwd=self.root)
        command("git", "config", "user.name", "Acceptance tooling test", cwd=self.root)
        command("git", "config", "user.email", "test@example.invalid", cwd=self.root)
        (self.root / "source.txt").write_text("original source\n")
        command("git", "add", "source.txt", cwd=self.root)
        command("git", "commit", "-qm", "test fixture", cwd=self.root)
        self.binary = self.directory / "acceptance"
        self.binary.write_bytes(b"tooling test binary contents, not a Base execution")
        self.metadata = self.directory / "nextest-binaries.json"
        self.metadata.write_text(json.dumps({"rust-binaries": {
            "base-system-tests::acceptance": {"binary-path": str(self.binary)},
        }}))
        self.provenance = {
            "source": source_state(self.root)[0],
            "binaries": binary_identities(self.metadata),
            "images": [],
        }
        (self.directory / "provenance.json").write_text(json.dumps(self.provenance))

    def test_unchanged_source_and_binary(self) -> None:
        verify(self.directory, self.root)

    def test_tracked_source_change(self) -> None:
        (self.root / "source.txt").write_text("changed source\n")
        with self.assertRaisesRegex(ValueError, "source changed"):
            verify(self.directory, self.root)

    def test_untracked_source_change(self) -> None:
        (self.root / "new.rs").write_text("new untracked source\n")
        with self.assertRaisesRegex(ValueError, "source changed"):
            verify(self.directory, self.root)

    def test_source_change_during_build(self) -> None:
        snapshot(self.directory, self.root)
        (self.root / "source.txt").write_text("changed while compiling\n")
        with self.assertRaisesRegex(ValueError, "source changed during preparation"):
            capture(self.directory, self.root, [], [])

    def test_binary_change(self) -> None:
        self.binary.write_bytes(b"different binary")
        with self.assertRaisesRegex(ValueError, "binary changed"):
            verify(self.directory, self.root)

    def test_wrong_or_missing_binary(self) -> None:
        self.metadata.write_text('{"rust-binaries": {}}')
        with self.assertRaisesRegex(ValueError, "expected only"):
            verify(self.directory, self.root)

    def test_snapshot_refuses_overwrite(self) -> None:
        snapshot(self.directory, self.root)
        with self.assertRaises(FileExistsError):
            snapshot(self.directory, self.root)


if __name__ == "__main__":
    if sys.argv[1:] == ["test"]:
        unittest.main(argv=[sys.argv[0]])
    else:
        main()
