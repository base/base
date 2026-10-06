#!/usr/bin/env python3
"""Offline launcher tests."""

import os
from pathlib import Path
import socket
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import snapshot_devnet as devnet


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.fork = devnet.SnapshotFork(self.root / "fork")
        self.fork.directory.mkdir()

    def datadir(self, name):
        path = self.root / name
        (path / "db").mkdir(parents=True)
        (path / "db/mdbx.dat").write_bytes(b"untouched")
        return path

    def preparation(self, suffix="", **changes):
        """Init input with fresh datadirs and a free L1 port rather than the default, which may be in use."""
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        config = {"sequencer_datadir": str(self.datadir("builder" + suffix)),
                  "validator_datadir": str(self.datadir("validator" + suffix)),
                  "port": port, **{role + "_image": role + ":local" for role in ("base", "anvil", "batcher")}}
        return {**config, **changes}

    def test_alias_and_nested_datadirs_rejected_without_writes(self):
        first, second = self.datadir("a"), self.datadir("b")
        alias = self.root / "alias"
        alias.symlink_to(first, target_is_directory=True)
        devnet.validate_paths(self.fork.directory, [first, second])
        for paths in ([first, alias], [first, first / "db"]):
            with self.assertRaisesRegex(RuntimeError, "distinct"):
                devnet.validate_paths(self.fork.directory, paths)
        with self.assertRaisesRegex(RuntimeError, "distinct"):
            devnet.validate_paths(self.root, [first, second])
        self.assertEqual((first / "db/mdbx.dat").read_bytes(), b"untouched")
        os.link(first / "db/mdbx.dat", self.root / "hardlink")
        with self.assertRaisesRegex(RuntimeError, "hard-linked"):
            devnet.validate_paths(self.fork.directory, [first, second])

    def test_datadirs_sharing_one_database_are_rejected(self):
        shared = self.datadir("shared")
        first, second = self.root / "first", self.root / "second"
        for path in (first, second):
            path.mkdir()
            (path / "db").symlink_to(shared / "db", target_is_directory=True)
        with self.assertRaisesRegex(RuntimeError, "outside its datadir"):
            devnet.validate_paths(self.fork.directory, [first, second])
        # Bind-mount aliases share a device and inode like this hard link, but keep a link count of one.
        third, fourth = self.datadir("third"), self.root / "fourth"
        (fourth / "db").mkdir(parents=True)
        os.link(third / "db/mdbx.dat", fourth / "db/mdbx.dat")
        with self.assertRaisesRegex(RuntimeError, "share one database"):
            devnet.validate_paths(self.fork.directory, [third, fourth])

    def test_occupied_port_leaves_no_state_so_init_can_retry_with_another(self):
        config = self.preparation()
        with socket.socket() as occupied, patch.object(
                devnet, "run", side_effect=lambda *args, **_: "sha256:" + "d" * 64 if args[0] == "docker" else "0x" + "1" * 40):
            occupied.bind(("127.0.0.1", 0))
            with self.assertRaises(OSError):
                devnet.SnapshotFork(self.fork.directory).prepare({**config, "port": occupied.getsockname()[1]})
            self.assertEqual(list(self.fork.directory.iterdir()), [])
            self.assertTrue(devnet.SnapshotFork(self.fork.directory).prepare(config))
        self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["port"], config["port"])

    def test_missing_image_reports_dockers_reason(self):
        missing = subprocess.CalledProcessError(1, "docker", stderr="Error response from daemon: No such image: base:local")
        with patch.object(devnet.subprocess, "run", side_effect=missing):
            with self.assertRaisesRegex(RuntimeError, "No such image: base:local"):
                self.fork.prepare(self.preparation())
        self.assertEqual(list(self.fork.directory.iterdir()), [])

    def test_obsolete_manual_fork_and_rollup_config_are_rejected_before_any_effect(self):
        for field in ("fork_block", "rollup_env"):
            with patch.object(devnet, "run") as run, patch.object(devnet, "validate_paths") as paths:
                with self.assertRaisesRegex(RuntimeError, f"{field} is obsolete"):
                    devnet.SnapshotFork(self.root / "new").prepare({field: 1})
                run.assert_not_called()
                paths.assert_not_called()

    def test_preparation_retry_preserves_identity_and_keys_and_rejects_changed_config(self):
        config = self.preparation()
        image = "sha256:" + "d" * 64
        with patch.object(devnet, "run", side_effect=lambda *args, **_: image if args[0] == "docker" else "0x" + args[-1][-40:]), \
                patch("builtins.print"):
            self.assertTrue(devnet.SnapshotFork(self.fork.directory).prepare(config))
            before = devnet.SnapshotFork(self.fork.directory).manifest
            keys_path = self.fork.directory / "keys.json"
            keys = keys_path.read_bytes()
            self.assertEqual(before["phase"], "inspecting")
            self.assertEqual(before["images"], {role: image for role in ("base", "anvil", "batcher")})
            self.assertEqual(set(before["accounts"]), {"batcher", "signer", "user"})
            for path in (keys_path, self.fork.directory / "manifest.json"):
                self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            self.assertTrue(devnet.SnapshotFork(self.fork.directory).prepare(config))
            self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest, before)
            self.assertEqual(keys_path.read_bytes(), keys)
            keys_path.unlink()
            with self.assertRaisesRegex(RuntimeError, "keys are missing"):
                devnet.SnapshotFork(self.fork.directory).prepare(config)
            keys_path.write_bytes(keys)

            completed = devnet.SnapshotFork(self.fork.directory)
            completed.manifest["phase"] = "prepared"
            completed.save()
            self.assertFalse(devnet.SnapshotFork(self.fork.directory).prepare(config))
            for changed in ({"epoch_slots": 99}, {"execution_env": "OTHER_EXECUTION"}):
                with self.subTest(changed=changed), self.assertRaisesRegex(RuntimeError, "config changed"):
                    devnet.SnapshotFork(self.fork.directory).prepare({**config, **changed})
            self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest, completed.manifest)

    def test_preparation_rejects_mutable_images_and_inline_credentials_before_writes(self):
        cases = (
            ({}, "base:local", "immutable local image IDs"),
            ({"execution_env": "https://rpc.invalid/key"}, "sha256:" + "d" * 64, "environment variable names"),
            ({"rollup_config": "{}"}, "sha256:" + "d" * 64, "unknown configuration fields"),
            ({"port": 80}, "sha256:" + "d" * 64, "unprivileged"),
        )
        for index, (changes, image, pattern) in enumerate(cases):
            fork = devnet.SnapshotFork(self.root / f"fork-{index}")
            fork.directory.mkdir()
            with self.subTest(pattern=pattern), patch.object(devnet, "run", return_value=image):
                with self.assertRaisesRegex(RuntimeError, pattern):
                    fork.prepare(self.preparation(str(index), **changes))
            self.assertEqual(list(fork.directory.iterdir()), [])

    def test_command_failure_is_actionable_without_endpoint_credentials(self):
        script = ("import sys; sys.stderr.write('history unavailable from https://user:pw@rpc.invalid/v2/k "
                  "and token-123'); sys.exit(1)")
        with self.assertRaises(RuntimeError) as caught:
            devnet.run(sys.executable, "-c", script, secrets=("token-123",))
        self.assertIn("history unavailable", str(caught.exception))
        for secret in ("rpc.invalid", "token-123", "pw@"):
            self.assertNotIn(secret, str(caught.exception))


if __name__ == "__main__":
    unittest.main()
