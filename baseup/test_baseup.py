#!/usr/bin/env python3
"""Offline installer tests. Run with python3 baseup/test_baseup.py."""

import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().with_name("baseup")
VERSION = "v1.2.3"
TARGET = "x86_64-unknown-linux-gnu"
ARCHIVE_NAME = f"base-{VERSION}-{TARGET}.tar.gz"
INSTALLED_CONTENT = b"#!/usr/bin/env bash\necho existing\n"
RELEASE_CONTENT = b"#!/usr/bin/env bash\necho installed\n"


class BaseupInstallTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="baseup-test-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.base_home = root / "base home"
        self.bin_dir = self.base_home / "bin"
        self.bin_dir.mkdir(parents=True)
        self.binary = self.bin_dir / "base"
        self.manifest = self.base_home / "installed-versions"
        self.download_log = root / "downloads"
        artifacts = root / "artifacts"
        artifacts.mkdir()
        archive = artifacts / ARCHIVE_NAME
        with tarfile.open(archive, "w:gz") as release:
            member = tarfile.TarInfo("base")
            member.size = len(RELEASE_CONTENT)
            member.mode = 0o755
            release.addfile(member, io.BytesIO(RELEASE_CONTENT))
        checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
        (artifacts / f"{ARCHIVE_NAME}.sha256").write_text(f"{checksum}  {ARCHIVE_NAME}\n")

        mock_bin = root / "mock-bin"
        mock_bin.mkdir()
        (mock_bin / "uname").write_text(
            '#!/usr/bin/env bash\n'
            'case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; *) exit 1 ;; esac\n'
        )
        (mock_bin / "curl").write_text(
            '#!/usr/bin/env bash\n'
            'set -eu\n'
            'while [[ $# -gt 0 ]]; do\n'
            '    case "$1" in\n'
            '        -o) output_path="$2"; shift 2 ;;\n'
            '        https://*) filename="${1##*/}"; shift ;;\n'
            '        *) shift ;;\n'
            '    esac\n'
            'done\n'
            'printf "%s\\n" "$filename" >> "$BASEUP_TEST_DOWNLOAD_LOG"\n'
            'cp "$BASEUP_TEST_ARTIFACTS/$filename" "$output_path"\n'
        )
        for mock in mock_bin.iterdir():
            mock.chmod(0o755)
        self.environment = os.environ.copy()
        self.environment.update(
            BASEUP_REPO="base/base",
            BASEUP_HOME=str(self.base_home),
            BASE_BIN_DIR=str(self.bin_dir),
            BASEUP_TEST_ARTIFACTS=str(artifacts),
            BASEUP_TEST_DOWNLOAD_LOG=str(self.download_log),
            PATH=str(mock_bin) + os.pathsep + self.environment["PATH"],
        )

    def write_installed(self, *, mode=0o755, version=VERSION, target=TARGET):
        self.binary.write_bytes(INSTALLED_CONTENT)
        self.binary.chmod(mode)
        self.manifest.write_text(f"base:{target}={version}\n")

    def run_installer(self, *arguments):
        result = subprocess.run(
            [os.environ.get("BASEUP_TEST_BASH", "/bin/bash"), str(INSTALLER),
             "--install", VERSION, "--bin", "base", "--unsafe-skip-verify", *arguments],
            env=self.environment, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def assert_reinstalled(self):
        self.assertEqual(self.binary.read_bytes(), RELEASE_CONTENT)
        self.assertTrue(os.access(self.binary, os.X_OK))
        self.assertEqual(
            self.download_log.read_text().splitlines(),
            [ARCHIVE_NAME, f"{ARCHIVE_NAME}.sha256"],
        )
        self.assertIn(f"base:{TARGET}={VERSION}", self.manifest.read_text().splitlines())

    def test_current_executable_skips_download(self):
        self.write_installed()
        self.run_installer()
        self.assertEqual(self.binary.read_bytes(), INSTALLED_CONTENT)
        self.assertFalse(self.download_log.exists())

    def test_non_executable_is_reinstalled(self):
        self.write_installed(mode=0o644)
        self.run_installer()
        self.assert_reinstalled()

    def test_missing_binary_is_reinstalled(self):
        self.write_installed()
        self.binary.unlink()
        self.run_installer()
        self.assert_reinstalled()

    def test_missing_manifest_is_reinstalled(self):
        self.write_installed()
        self.manifest.unlink()
        self.run_installer()
        self.assert_reinstalled()

    def test_old_version_is_reinstalled(self):
        self.write_installed(version="v1.2.2")
        self.run_installer()
        self.assert_reinstalled()

    def test_other_target_is_reinstalled(self):
        self.write_installed(target="aarch64-unknown-linux-gnu")
        self.run_installer()
        self.assert_reinstalled()

    def test_force_reinstalls_current_executable(self):
        self.write_installed()
        self.run_installer("--force")
        self.assert_reinstalled()


if __name__ == "__main__":
    unittest.main()
