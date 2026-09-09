#!/usr/bin/env python3
"""Prepare pinned contract artifacts once, with locked, content-checked cache reuse."""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


class ContractBuild:
    """Owns the disposable artifact cache, not genesis or node data directories."""

    INPUTS = (
        "etc/upstream-pins/contracts.rev",
        "etc/docker/Dockerfile.rust-services",
        "etc/scripts/devnet/export-contracts.py",
        "etc/scripts/devnet/contracts.py",
    )
    MANIFEST = ".build-manifest"
    REQUIRED = {"revision.txt", "preinstalls.json", "predeploys.json", "ProtocolVersions.json"}

    def __init__(self, root, output):
        self.root = Path(root)
        self.output = Path(output)

    @staticmethod
    def digest(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    def inputs(self):
        return {name: self.digest(self.root / name) for name in self.INPUTS}

    def files(self, directory):
        files = {}
        for path in directory.iterdir():
            if path.name == self.MANIFEST:
                continue
            if not path.is_file() or path.is_symlink():
                raise ValueError(f"unexpected contract artifact: {path}")
            files[path.name] = self.digest(path)
        if not self.REQUIRED.issubset(files):
            raise ValueError(f"incomplete contract artifacts in {directory}")
        revision = (self.root / self.INPUTS[0]).read_text().strip()
        if (directory / "revision.txt").read_text().strip() != revision:
            raise ValueError("contract artifact revision does not match the pin")
        return files

    def current(self, inputs):
        try:
            manifest = json.loads((self.output / self.MANIFEST).read_text())
            return manifest["inputs"] == inputs and manifest["files"] == self.files(self.output)
        except (OSError, ValueError, KeyError, TypeError):
            return False

    def prepare(self, force=False):
        self.output.parent.mkdir(parents=True, exist_ok=True)
        lock_path = self.output.parent / f".{self.output.name}.build.lock"
        with lock_path.open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            inputs = self.inputs()
            if not force and self.current(inputs):
                return
            with tempfile.TemporaryDirectory(prefix=".contracts-export-", dir=self.output.parent) as temp:
                staging = Path(temp)
                subprocess.run(
                    ["docker", "buildx", "build", "-f", "etc/docker/Dockerfile.rust-services",
                     "--target", "contracts-artifacts", "--output", f"type=local,dest={staging}", "."],
                    cwd=self.root, check=True,
                )
                exported = staging / "artifacts"
                files = self.files(exported)
                if self.inputs() != inputs:
                    raise ValueError("contract build inputs changed during export; retry preparation")
                (exported / self.MANIFEST).write_text(
                    json.dumps({"inputs": inputs, "files": files}, sort_keys=True) + "\n"
                )
                # Preserve the previous cache on build failure. Publication and cleanup
                # happen only after the entire replacement has been checked.
                previous = staging / "previous"
                if self.output.exists():
                    os.rename(self.output, previous)
                try:
                    os.rename(exported, self.output)
                except OSError:
                    if previous.exists():
                        os.rename(previous, self.output)
                    raise


if __name__ == "__main__":
    root = Path(__file__).resolve().parents[3]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--force", action="store_true", help="rebuild even if the cache is current")
    args = parser.parse_args()
    try:
        ContractBuild(root, root / ".contracts/artifacts").prepare(args.force)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"contract preparation failed: {error}", file=sys.stderr)
        sys.exit(1)
