#!/usr/bin/env python3
"""Offline launcher tests; optional live qualification requires BASE_SNAPSHOT_FORK_DIR."""

import contextlib
import copy
import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import DEFAULT, patch

import snapshot_devnet as devnet
import snapshot_verify as verification


def snapshot():
    head = {"block_info": {"number": 123, "hash": "0x123", "timestamp": 1234,
                           "l1origin": {"number": 19, "hash": "0x19"}},
            "system_config": {"batcherAddr": "0xb"}}
    return {"chain_id": 8453, "rollup_config": {"l1_chain_id": 1},
            "latest": copy.deepcopy(head), "safe": copy.deepcopy(head), "finalized": copy.deepcopy(head)}


def manifest():
    return {
        "version": 2, "project": "snapshot-fixture", "phase": "prepared", "port": 19545,
        "fork": {"number": "0x64", "hash": "0xf", "timestamp": "0x4d8", "parentHash": "0xe"},
        "images": {name: "sha256:" + "a" * 64 for name in ("base", "anvil", "batcher")},
        "datadirs": {"sequencer": "/unused-sequencer", "validator": "/unused-validator"},
        "epoch_slots": 3, "slot_seconds": 12, "protocol_versions": devnet.PROTOCOL_VERSIONS,
        "beacon_genesis": {"genesis_time": "1000"},
        "upstreams": {"execution": "TEST_EXECUTION_URL", "beacon": "TEST_BEACON_URL"},
        "accounts": {"batcher": "0x" + "b" * 40, "signer": "0x" + "c" * 40, "user": "0x" + "d" * 40},
        "operations": {},
        "contracts": {},
    }


def container(service, address="10.9.0.2", project="snapshot-fixture", networks=None, running=True):
    """One `docker inspect` record for a Compose-managed container."""
    return {
        "Config": {"Labels": {"com.docker.compose.project": project, "com.docker.compose.service": service}},
        "State": {"Running": running},
        "NetworkSettings": {"Networks": networks if networks is not None
                            else {project + "_private": {"IPAddress": address}}},
    }


def sync_status(l1, safe=123, safe_hash="0x123", unsafe=None, unsafe_hash=None):
    return {"current_l1": {"number": l1, "hash": f"0xl1{l1}"},
            "safe_l2": {"number": safe, "hash": safe_hash},
            "unsafe_l2": {"number": safe if unsafe is None else unsafe,
                          "hash": safe_hash if unsafe_hash is None else unsafe_hash,
                          "timestamp": 1234, "l1origin": {"number": 19}}}


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        environment = patch.dict(os.environ, {"XDG_CONFIG_HOME": str(self.root / "user-config")})
        environment.start()
        self.addCleanup(environment.stop)
        self.fork = devnet.SnapshotFork(self.root / "fork", timeout=1)
        self.fork.directory.mkdir()
        self.fork.manifest = manifest()

    def datadir(self, name):
        path = self.root / name
        (path / "db").mkdir(parents=True)
        (path / "db/mdbx.dat").write_bytes(b"untouched")
        return path

    def test_up_and_down_dispatch_to_nondestructive_lifecycle(self):
        self.fork.save()
        for command, method in (("up", "start"), ("down", "stop")):
            with self.subTest(command=command), \
                    patch.object(sys, "argv", ["snapshot_devnet.py", command, "--dir", str(self.fork.directory)]), \
                    patch.object(devnet.SnapshotFork, method) as lifecycle:
                devnet.main()
                lifecycle.assert_called_once_with()

    def test_up_requires_completed_setup_before_any_service_operation(self):
        with patch.object(sys, "argv", ["launcher", "up"]), patch.object(devnet, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "run just devnet snapshot setup first"):
                devnet.main()
            path = devnet.setup_path()
            path.parent.mkdir(parents=True)
            devnet.write_json(path, {"directory": str(self.root / "missing")})
            with self.assertRaisesRegex(RuntimeError, "saved snapshot directory is unavailable"):
                devnet.main()
            run.assert_not_called()

    def test_setup_selects_existing_fork_and_reuses_private_credentials_without_dir(self):
        self.fork.save()
        before = (self.fork.directory / "manifest.json").read_bytes()
        devnet.setup_path().parent.mkdir(parents=True)
        (devnet.setup_path().parent / "l1.env").write_text("export ETH_L1_RPC='https://rpc.invalid/secret-key'\n")
        with patch.object(sys, "argv", ["launcher", "setup", "--dir", str(self.fork.directory)]), \
                patch.object(devnet.getpass, "getpass", side_effect=AssertionError("configured endpoint must not prompt")), \
                patch.object(devnet, "rpc", return_value="0x1"), \
                patch.object(devnet, "request_json", side_effect=[{"data": {"genesis_time": "1000"}},
                                                                 {"data": {"SECONDS_PER_SLOT": "12"}}]), \
                patch.object(devnet, "setup_command") as command, patch("builtins.print") as output:
            devnet.main()
            command.assert_not_called()
        self.assertEqual(devnet.configured_directory(), self.fork.directory)
        self.assertEqual((self.fork.directory / "manifest.json").read_bytes(), before)
        self.assertEqual((self.fork.directory / "upstreams.json").stat().st_mode & 0o777, 0o600)
        self.assertNotIn("secret-key", devnet.setup_path().read_text() + str(output.call_args_list))
        for variable in ("TEST_EXECUTION_URL", "TEST_BEACON_URL", "ETH_L1_RPC"):
            os.environ.pop(variable, None)
        (devnet.setup_path().parent / "l1.env").unlink()
        with patch.object(sys, "argv", ["launcher", "setup"]), \
                patch("builtins.input", side_effect=AssertionError("saved directory must not prompt")), \
                patch.object(devnet.getpass, "getpass", side_effect=AssertionError("saved endpoint must not prompt")), \
                patch.object(devnet, "rpc", return_value="0x1") as rpc, \
                patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                patch.object(devnet, "prepare_snapshot") as prepare, patch("builtins.print"):
            devnet.main()
            rpc.assert_called_once_with("https://rpc.invalid/secret-key", "eth_chainId", upstream=True)
            prepare.assert_not_called()
        for action, method in (("up", "start"), ("down", "stop")):
            with self.subTest(action=action), patch.object(sys, "argv", ["launcher", action]), \
                    patch.object(devnet.SnapshotFork, method, autospec=True) as lifecycle:
                devnet.main()
                fork = lifecycle.call_args.args[0]
                self.assertEqual(fork.directory, self.fork.directory)
                self.assertEqual(fork.timeout, 1800)
                self.assertEqual(fork.endpoint("execution"), "https://rpc.invalid/secret-key")
        with patch.object(sys, "argv", ["verify"]), patch.object(verification, "verify") as verify:
            verification.main()
            self.assertEqual(verify.call_args.args[0].directory, self.fork.directory)
            self.assertEqual(verify.call_args.args[0].timeout, 1800)
        devnet.write_json(self.fork.directory / "keys.json", {"signer": "0x1", "batcher": "0x2"})
        fork = devnet.SnapshotFork(self.fork.directory)
        self.assertEqual(fork.compose_env()["SNAPSHOT_BEACON"], "https://rpc.invalid/secret-key")

    def test_setup_refuses_existing_data_and_wrong_chain_without_downloads(self):
        for directory, chain, error in ((self.root, "0x1", "unused working directory"),
                                        (self.root / "new", "0xa", "Ethereum mainnet")):
            with self.subTest(directory=directory), \
                    patch.object(sys, "argv", ["launcher", "setup", "--workdir", str(directory)]), \
                    patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                    patch.object(devnet, "rpc", return_value=chain), \
                    patch.object(devnet, "setup_command") as command:
                with self.assertRaisesRegex(RuntimeError, error):
                    devnet.main()
                command.assert_not_called()
                self.assertFalse(devnet.setup_path().exists())
        self.assertFalse((self.root / "new").exists())

    def test_setup_rejects_nonpositive_download_concurrency_before_prompting(self):
        for concurrency in ("0", "-1"):
            with self.subTest(concurrency=concurrency), \
                    patch.object(sys, "argv", ["launcher", "setup", "--download-concurrency", concurrency]), \
                    patch("builtins.input") as prompt, patch.object(devnet, "setup_command") as command:
                with self.assertRaisesRegex(RuntimeError, "download concurrency must be positive"):
                    devnet.main()
                prompt.assert_not_called()
                command.assert_not_called()

    def test_setup_downloads_once_copies_with_progress_and_pins_images_before_init(self):
        home = self.root / "home with spaces"
        for failed in (None, "download", "copy"):
            with self.subTest(failed=failed):
                work = self.root / f"{failed} experiment" if failed else home / "data/snapshot-devnet"
                commands = []
                copy_output = []
                image = "sha256:" + "b" * 64

                def command(*args, lock_fd):
                    commands.append(args)
                    if args[:2] == ("docker", "run"):
                        builds = [command for command in commands if command[:3] == ("docker", "buildx", "bake")]
                        self.assertEqual(len(builds), 1, "always build this checkout before downloading, even with cached images")
                        self.assertIn("etc/docker/docker-bake.hcl", builds[0])
                        self.assertIn("base", builds[0])
                        self.assertIn("base.args.PROFILE=release", builds[0])
                        self.assertIn("--load", builds[0])
                        self.assertNotIn(("docker", "pull", "ghcr.io/base/node:main"), commands)
                        self.assertIn(image, args)
                        self.assertEqual(args[args.index("--manifest-path") + 1], "/work/download-manifest.json")
                        # The published full snapshot retains 31 days of pre-Denim history.
                        # --full instead selects 10x as many blocks under Denim's production defaults.
                        self.assertNotIn("--full", args)
                        for flag in ("--with-txs-distance", "--with-receipts-distance", "--with-state-history-distance"):
                            self.assertEqual(args[args.index(flag) + 1], "1339200")
                        self.assertEqual(args[args.index("--download-concurrency") + 1], "32" if failed else "16")
                        self.assertNotIn("--force", args)
                        (work / "builder/db").mkdir(parents=True)
                        (work / "builder/db/mdbx.dat").write_bytes(b"snapshot-state")
                        (work / "builder/static_files").mkdir()
                        (work / "builder/static_files/headers").write_bytes(b"snapshot-history")
                        if failed == "download":
                            raise RuntimeError("download interrupted")
                    elif args[0] in ("cp", "rsync"):
                        if failed == "copy":
                            (work / "validator/db").mkdir(parents=True)
                            (work / "validator/db/mdbx.dat").write_bytes(b"partial")
                            raise RuntimeError("copy interrupted")
                        result = subprocess.run(args, check=True, capture_output=True, text=True)
                        copy_output.append(result.stdout)

                def initialize(fork, config, allow_write):
                    self.assertTrue(allow_write)
                    fork.manifest = manifest()
                    fork.manifest["upstreams"] = {"execution": "SNAPSHOT_UPSTREAM_EXECUTION",
                                                  "beacon": "SNAPSHOT_UPSTREAM_BEACON"}
                    self.assertEqual(fork.endpoint("execution"), "https://rpc.invalid/new-key")
                    for role in ("base", "anvil", "batcher"):
                        self.assertEqual(config[role + "_image"], image)
                    for role in ("builder", "validator"):
                        self.assertEqual((work / role / "static_files/headers").read_bytes(), b"snapshot-history")
                    self.assertNotEqual((work / "builder/db/mdbx.dat").stat().st_ino,
                                        (work / "validator/db/mdbx.dat").stat().st_ino)
                    (work / "validator/db/mdbx.dat").write_bytes(b"validator-only-write")
                    self.assertEqual((work / "builder/db/mdbx.dat").read_bytes(), b"snapshot-state")
                    fork.save()

                options = ["--workdir", str(work), "--download-concurrency", "32"] if failed else []
                with patch.object(sys, "argv", ["launcher", "setup", *options]), \
                        patch.object(Path, "home", return_value=home), patch("builtins.input", return_value=""), \
                        patch.object(devnet.getpass, "getpass", return_value="https://beacon.invalid/key") as prompt, \
                        patch.dict(os.environ, {"SNAPSHOT_UPSTREAM_EXECUTION": "https://rpc.invalid/new-key",
                                                "SNAPSHOT_UPSTREAM_BEACON": ""}), \
                        patch.object(devnet, "rpc", return_value="0x1"), \
                        patch.object(devnet, "request_json", side_effect=[devnet.Unavailable("no Beacon API"),
                            {"data": {"genesis_time": "1000"}}, {"data": {"SECONDS_PER_SLOT": "12"}},
                            [{"chainId": 8453, "block": 123, "metadataUrl": "https://snapshot.invalid/123/manifest.json"}],
                            {"chain_id": 8453, "block": 123}]), \
                        patch.object(devnet.shutil, "which", return_value="/usr/bin/tool"), \
                        patch.object(devnet, "run", return_value=image), \
                        patch.object(devnet, "setup_command", side_effect=command), \
                        patch.object(devnet.SnapshotFork, "initialize", autospec=True, side_effect=initialize) as init, \
                        patch("builtins.print"):
                    if failed:
                        saved = json.loads(devnet.setup_path().read_text())["directory"]
                        with self.assertRaisesRegex(RuntimeError, f"{failed} interrupted"):
                            devnet.main()
                        init.assert_not_called()
                        self.assertEqual(json.loads(devnet.setup_path().read_text())["directory"], saved)
                        self.assertEqual(json.loads(devnet.setup_path().read_text())["pending_directory"], str(work / "fork"))
                        if failed == "copy":
                            self.assertEqual((work / "validator/db/mdbx.dat").read_bytes(), b"partial")
                        else:
                            self.assertFalse((work / "validator").exists())
                    else:
                        devnet.main()
                        self.assertEqual(devnet.configured_directory(), work / "fork")
                        init.assert_called_once()
                    self.assertEqual(sum(args[:2] == ("docker", "run") for args in commands), 1)
                    prompt.assert_called_once()
                    self.assertEqual((work / "builder/db/mdbx.dat").read_bytes(), b"snapshot-state")
                    self.assertNotIn("new-key", (work / "input.json").read_text())
                    if not failed:
                        self.assertEqual(len(copy_output), 1)
                        self.assertIn("100%", copy_output[0], "the copy must report progress after the download exits")

    def test_setup_resumes_each_phase_without_changing_snapshot_or_recopying_opened_data(self):
        for failure in ("inspector", "download", "copy", "initialize"):
            with self.subTest(failure=failure):
                work = self.root / failure
                calls, initialized, fetched = [], [], []
                interrupted = False
                image = "sha256:" + "e" * 64

                def fail(phase):
                    nonlocal interrupted
                    if phase == failure and not interrupted:
                        interrupted = True
                        raise KeyboardInterrupt()

                def metadata(url):
                    fetched.append(url)
                    if url.endswith("/genesis"):
                        return {"data": {"genesis_time": "1000"}}
                    if url.endswith("/spec"):
                        return {"data": {"SECONDS_PER_SLOT": "12"}}
                    if url == devnet.SNAPSHOT_INDEX:
                        self.assertEqual(fetched.count(url), 1, "retry must not select a newer snapshot")
                        return [{"chainId": "8453", "block": "123", "metadataUrl": "https://snapshot.invalid/123/manifest.json"},
                                {"chainId": 8453, "block": 90, "metadataUrl": "https://snapshot.invalid/90/manifest.json"},
                                {"chainId": 1, "block": 999, "metadataUrl": "https://snapshot.invalid/999/manifest.json"}]
                    self.assertEqual(url, "https://snapshot.invalid/123/manifest.json")
                    return {"chain_id": 8453, "block": 123}

                def command(*args, lock_fd):
                    if args[0] == "cargo":
                        calls.append("inspector")
                        fail("inspector")
                    elif args[:3] == ("docker", "buildx", "bake"):
                        calls.append("build")
                    elif args[:2] == ("docker", "run"):
                        calls.append("download")
                        self.assertEqual(args[args.index("--manifest-path") + 1], "/work/download-manifest.json")
                        pinned = json.loads((work / "download-manifest.json").read_text())
                        self.assertEqual(pinned, {"chain_id": 8453, "block": 123, "base_url": "https://snapshot.invalid/123/"})
                        fail("download")
                        (work / "builder/db").mkdir(parents=True)
                        (work / "builder/db/mdbx.dat").write_bytes(b"snapshot")
                    elif args[0] == "rsync":
                        calls.append("copy")
                        (work / "validator/db").mkdir(parents=True, exist_ok=True)
                        (work / "validator/db/mdbx.dat").write_bytes(b"partial")
                        fail("copy")
                        subprocess.run(args, check=True, capture_output=True)

                def initialize(fork, config, allow_write):
                    initialized.append(config)
                    fork.manifest = {**manifest(), "phase": "inspecting", "setup_input": config}
                    fork.save()
                    # If init has touched either datadir, no retry may copy builder onto validator.
                    (work / "validator/db/mdbx.dat").write_bytes(b"opened-by-inspection")
                    fail("initialize")
                    fork.manifest["phase"] = "prepared"
                    fork.save()

                with patch.object(sys, "argv", ["launcher", "setup", "--workdir", str(work)]), \
                        patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                        patch.object(devnet, "rpc", return_value="0x1"), \
                        patch.object(devnet, "request_json", side_effect=metadata), \
                        patch.object(devnet, "run", return_value=image), \
                        patch.object(devnet.shutil, "which", return_value="/usr/bin/tool"), \
                        patch.object(devnet, "setup_command", side_effect=command), \
                        patch.object(devnet.SnapshotFork, "initialize", autospec=True, side_effect=initialize), \
                        patch("builtins.input", side_effect=AssertionError("resume must remember its workdir")), \
                        patch("builtins.print"):
                    with self.assertRaises(KeyboardInterrupt):
                        devnet.main()
                    with patch.object(sys, "argv", ["launcher", "setup"]):
                        devnet.main()
                        before = list(calls), len(initialized)
                        devnet.main()
                        self.assertEqual((calls, len(initialized)), before, "completed setup must do no preparation")
                self.assertEqual(calls.count("build"), 1)
                for phase in ("inspector", "download", "copy"):
                    self.assertEqual(calls.count(phase), 2 if phase == failure else 1)
                self.assertEqual(len(initialized), 2 if failure == "initialize" else 1)
                self.assertEqual((work / "builder/db/mdbx.dat").read_bytes(), b"snapshot")
                self.assertEqual((work / "validator/db/mdbx.dat").read_bytes(), b"opened-by-inspection")
                self.assertEqual(devnet.configured_directory(), work / "fork")
                self.assertNotIn("pending_directory", json.loads(devnet.setup_path().read_text()))

    def test_setup_adopts_legacy_download_only_with_confirmation_and_never_redownloads(self):
        work = self.root / "legacy"
        builder = self.datadir("legacy/builder")
        (builder / "reth.toml").write_text("# generated by downloader")
        image = "sha256:" + "b" * 64
        config = {"sequencer_datadir": str(builder), "validator_datadir": str(work / "validator"),
                  "port": 19545, **{role + "_image": image for role in ("base", "anvil", "batcher")}}
        devnet.write_json(work / "input.json", config)
        devnet.setup_path().parent.mkdir(parents=True)
        devnet.write_json(devnet.setup_path(), {"directory": str(work)})

        def command(*args, lock_fd):
            self.assertEqual(args[0], "rsync", "adoption must not rebuild or invoke the downloader")
            subprocess.run(args, check=True, capture_output=True)

        def initialize(fork, saved, allow_write):
            self.assertEqual(saved, config)
            self.assertEqual((work / "validator/db/mdbx.dat").read_bytes(), b"untouched")
            fork.manifest = manifest()
            fork.save()

        with patch.object(sys, "argv", ["launcher", "setup"]), \
                patch.object(Path, "home", return_value=self.root / "home"), \
                patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                patch.object(devnet, "rpc", return_value="0x1"), \
                patch.object(devnet, "request_json", side_effect=lambda url: {
                    "data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                patch.object(devnet, "setup_command", side_effect=command) as commands, \
                patch.object(devnet.SnapshotFork, "initialize", autospec=True, side_effect=initialize), \
                patch("builtins.print"):
            with patch("builtins.input", return_value=""):
                with self.assertRaisesRegex(RuntimeError, "not confirmed"):
                    devnet.main()
            commands.assert_not_called()
            with patch("builtins.input", return_value="y"):
                devnet.main()
            commands.assert_called_once()
        self.assertEqual(devnet.configured_directory(), work / "fork")

    def test_setup_validates_saved_endpoints_without_prompting_for_replacements(self):
        self.fork.save()
        devnet.write_json(self.fork.directory / "upstreams.json", {
            "execution": "https://rpc.invalid/key", "beacon": "https://beacon.invalid/key"})
        for name, chain, metadata in (("wrong chain", "0xa", {}),
                                      ("unreachable RPC", devnet.Unavailable("offline"), {}),
                                      ("invalid Beacon", "0x1", {"data": {"genesis_time": "0"}})):
            with self.subTest(name=name), \
                    patch.object(sys, "argv", ["launcher", "setup", "--dir", str(self.fork.directory)]), \
                    patch.object(devnet.getpass, "getpass", side_effect=AssertionError("configured endpoints must not prompt")), \
                    patch.object(devnet, "rpc", side_effect=chain if isinstance(chain, Exception) else lambda *a, **k: chain), \
                    patch.object(devnet, "request_json", return_value=metadata), \
                    patch.object(devnet, "setup_command") as command:
                with self.assertRaises(RuntimeError):
                    devnet.main()
                command.assert_not_called()
        self.assertFalse(devnet.setup_path().exists())

    def test_setup_lock_prevents_concurrent_preparation(self):
        self.fork.save()
        with open(self.fork.directory / ".lock", "a") as lock, \
                patch.object(sys, "argv", ["launcher", "setup", "--dir", str(self.fork.directory)]), \
                patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                patch.object(devnet, "rpc", return_value="0x1"), \
                patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                patch.object(devnet, "prepare_snapshot") as prepare:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaises(BlockingIOError):
                devnet.main()
            prepare.assert_not_called()
        self.assertFalse((self.fork.directory / "upstreams.json").exists())

    def test_setup_commands_inherit_the_fork_lock(self):
        with open(self.fork.directory / ".lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            devnet.setup_command(
                sys.executable, "-c",
                "import os, sys; assert os.fstat(int(sys.argv[1])).st_ino == int(sys.argv[2])",
                str(lock.fileno()), str(os.fstat(lock.fileno()).st_ino), lock_fd=lock.fileno())

    def test_init_retry_preserves_identity_and_keys_and_completed_init_is_a_noop(self):
        builder, validator = self.datadir("builder"), self.datadir("validator")
        image = "sha256:" + "d" * 64
        config = {"sequencer_datadir": str(builder), "validator_datadir": str(validator),
                  **{role + "_image": image for role in ("base", "anvil", "batcher")}}
        config_path = self.root / "input.json"
        devnet.write_json(config_path, config)
        initial = snapshot()
        initial["rollup_config"]["l1_system_config_address"] = "0x" + "1" * 40
        discovered = {"number": 100, "hash": "0xf", "timestamp": 1240, "parentHash": "0xe"}
        header = {"number": "0x64", "hash": "0xf", "timestamp": "0x4d8", "parentHash": "0xe"}

        def transport(url, method, *args, **kwargs):
            if method == "eth_chainId":
                return "0x1"
            if method == "eth_getBlockByNumber":
                return {"number": "0x13", "hash": "0x19"} if args[0] == "0x13" else header
            return "0x01"

        def contract(url, address, signature, *args, **kwargs):
            if signature == "getSchedule()(uint64[])":
                return [0] * 14
            if signature == "minimumProtocolVersion()(uint256)":
                return 1
            return "0x" + "2" * 40

        with patch.object(sys, "argv", ["launcher", "init", "--dir", str(self.fork.directory),
                                       "--config", str(config_path), "--allow-write"]), \
                patch.object(devnet, "run", side_effect=lambda *args: image if args[0] == "docker" else "0x" + args[-1][-40:]), \
                patch.dict(os.environ, {"SNAPSHOT_UPSTREAM_EXECUTION": "https://rpc.invalid/key",
                                        "SNAPSHOT_UPSTREAM_BEACON": "https://rpc.invalid/key"}), \
                patch.object(devnet, "rpc", side_effect=transport), \
                patch.object(devnet, "call", side_effect=contract), \
                patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                patch.object(devnet.SnapshotFork, "inspect", side_effect=[KeyboardInterrupt(),
                    [{**copy.deepcopy(initial), "fork": discovered}, copy.deepcopy(initial)]]) as inspect, \
                patch("builtins.print"):
            with self.assertRaises(KeyboardInterrupt):
                devnet.main()
            before = devnet.SnapshotFork(self.fork.directory).manifest
            keys = (self.fork.directory / "keys.json").read_bytes()
            self.assertEqual(before["phase"], "inspecting")
            devnet.main()
            after = devnet.SnapshotFork(self.fork.directory).manifest
            self.assertEqual(after["phase"], "prepared")
            self.assertEqual(after["project"], before["project"])
            self.assertEqual(after["accounts"], before["accounts"])
            self.assertEqual((self.fork.directory / "keys.json").read_bytes(), keys)
            devnet.main()
            self.assertEqual(inspect.call_count, 2)
            self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest, after)
            devnet.write_json(config_path, {**config, "epoch_slots": 99})
            with self.assertRaisesRegex(RuntimeError, "config changed"):
                devnet.main()
            self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest, after)

    def test_up_reuses_running_services_and_recovers_partial_start_before_inspection(self):
        self.fork.manifest["phase"] = "running"
        with patch.object(self.fork, "running", return_value=True), \
                patch.object(self.fork, "running_services", return_value=set(devnet.FORK_SERVICES)) as services, \
                patch.object(self.fork, "assert_local_l1"), \
                patch.object(self.fork, "validate_restored_contracts"), \
                patch.object(self.fork, "consensus_ready", return_value=True), \
                patch.object(self.fork, "url", return_value="http://sequencer-cl"), \
                patch.object(devnet, "rpc", return_value=True) as active, \
                patch.object(self.fork, "schedule_denim") as schedule, \
                patch.object(self.fork, "stop") as stop, \
                patch.object(self.fork, "inspect") as inspect, \
                patch.object(devnet, "validate_paths", side_effect=RuntimeError("recovery reached")) as paths, \
                patch("builtins.print"):
            self.fork.start()
            schedule.assert_called_once()
            stop.assert_not_called()
            paths.assert_not_called()
            inspect.assert_not_called()
            # A restarted container can be alive while its sequencer is still stopped.
            active.return_value = False
            with self.assertRaisesRegex(RuntimeError, "recovery reached"):
                self.fork.start()
            stop.assert_called_once()
            stop.reset_mock()
            services.return_value = {"l1", "sequencer"}
            with self.assertRaisesRegex(RuntimeError, "recovery reached"):
                self.fork.start()
            stop.assert_called_once()
            inspect.assert_not_called()

    @unittest.skipUnless(shutil.which("just"), "requires the just command dispatcher")
    def test_nested_just_commands_forward_arguments_without_starting_services(self):
        for command in ("setup", "init", "up", "down", "start", "stop", "status", "reset",
                        "schedule-denim", "deposit", "verify"):
            with self.subTest(command=command):
                result = subprocess.run(
                    ["just", "devnet", "snapshot", command, "--dir", str(self.root / "fork with spaces"), "--help"],
                    cwd=devnet.ROOT, capture_output=True, text=True, timeout=10,
                    env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("usage:", result.stdout)
                if command == "setup":
                    self.assertNotIn("--base-image", result.stdout)
                self.assertFalse((self.root / "fork with spaces").exists())
        result = subprocess.run(
            ["just", "devnet", "snapshot", "status", "--dir", str(self.root / "fork with spaces"), "--timeout", "1"],
            cwd=devnet.ROOT, capture_output=True, text=True, timeout=10,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fork directory does not exist", result.stderr)

    @unittest.skipUnless(shutil.which("just"), "requires the just command dispatcher")
    def test_snapshot_is_a_just_module_with_per_task_help(self):
        for args in (["devnet", "snapshot"], ["--list", "devnet", "snapshot"]):
            with self.subTest(args=args):
                result = subprocess.run(["just", *args], cwd=devnet.ROOT,
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                for command in ("setup", "up", "down", "status", "test", "verify", "build-anvil"):
                    self.assertIn(command, result.stdout)
                self.assertNotIn("snapshot-test", result.stdout)
                self.assertNotIn("snapshot-verify", result.stdout)

    def test_slot_grid_skips_downtime_without_reusing_future_tip(self):
        self.assertEqual(devnet.next_slot(1000, 12, 1240, 1293), 1300)
        self.assertEqual(devnet.next_slot(1000, 12, 1312, 1293), 1324)
        self.assertEqual(devnet.next_slot(1000, 12, 1240, 1252), 1252)
        with self.assertRaises(RuntimeError):
            devnet.next_slot(1000, 0, 1240, 1252)

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

    def test_mining_waits_for_current_slot_and_does_not_warp_a_future_tip(self):
        calls = []
        self.fork.manifest["slot_seconds"] = 12
        with patch.object(self.fork, "assert_local_l1"), patch.object(devnet.time, "time", return_value=1293), \
                patch.object(devnet.time, "sleep", side_effect=lambda seconds: calls.append(("sleep", seconds))), \
                patch.object(devnet, "rpc", side_effect=lambda _, method, *args: calls.append((method, *args)) or {"timestamp": hex(1240)}):
            self.fork.mine()
        self.assertEqual(calls[-2:], [("sleep", 7), ("evm_mine", {"timestamp": 1300})])
        with patch.object(self.fork, "assert_local_l1"), patch.object(devnet.time, "time", return_value=1293), \
                patch.object(devnet, "rpc", return_value={"timestamp": hex(1312)}), \
                patch.object(devnet.time, "sleep") as sleep:
            with self.assertRaisesRegex(RuntimeError, "ahead of wall clock"):
                self.fork.mine()
            sleep.assert_not_called()

    def test_boundary_requires_matching_snapshots_with_origins_before_fork(self):
        initial = snapshot()
        devnet.validate_boundary([initial, copy.deepcopy(initial)], 19)
        with self.assertRaisesRegex(RuntimeError, "origin is after the fork"):
            devnet.validate_boundary([initial, initial], 18)
        other = copy.deepcopy(initial)
        other["latest"]["system_config"]["batcherAddr"] = "0xother"
        with self.assertRaisesRegex(RuntimeError, "heads or system configs"):
            devnet.validate_boundary([initial, other], 19)

    def test_discovered_fork_must_match_canonical_finalized_header_on_slot_grid(self):
        fork = {"number": 100, "hash": "0xf", "timestamp": 1240, "parentHash": "0xe"}
        header = {"number": "0x64", "hash": "0xF", "timestamp": hex(1240), "parentHash": "0xe"}
        devnet.validate_fork(fork, header, {"number": "0x64"}, 1000, 12)
        for changed, finalized, pattern in (
            ({"hash": "0xother"}, "0x65", "not canonical"),
            ({"parentHash": "0xother"}, "0x65", "not canonical"),
            ({"timestamp": hex(1241)}, "0x65", "not canonical"),
            ({}, "0x63", "not finalized"),
        ):
            with self.assertRaisesRegex(RuntimeError, pattern):
                devnet.validate_fork(fork, {**header, **changed}, {"number": finalized}, 1000, 12)
        with self.assertRaisesRegex(RuntimeError, "not canonical"):
            devnet.validate_fork(fork, None, {"number": "0x65"}, 1000, 12)
        off_grid = {**fork, "timestamp": 1241}
        with self.assertRaisesRegex(RuntimeError, "slot grid"):
            devnet.validate_fork(off_grid, {**header, "timestamp": hex(1241)}, {"number": "0x65"}, 1000, 12)

    def test_obsolete_manual_fork_and_rollup_config_are_rejected_before_any_effect(self):
        for field in ("fork_block", "rollup_env"):
            with patch.object(devnet, "run") as run, patch.object(devnet, "validate_paths") as paths:
                with self.assertRaisesRegex(RuntimeError, f"{field} is obsolete"):
                    devnet.SnapshotFork(self.root / "new").initialize({field: 1}, allow_write=True)
                run.assert_not_called()
                paths.assert_not_called()

    def test_historical_schedule_cannot_be_cleared_or_retroactively_enabled(self):
        config = {"genesis": {"l2_time": 100}, "regolith_time": 0,
                  "canyon_time": 200, "base": {"cobalt": 800}}
        schedule = [100, 200] + [0] * 10 + [800]
        devnet.validate_schedule(config, schedule, 300)
        for index, value in ((0, 0), (1, 201), (12, 250)):
            changed = list(schedule)
            changed[index] = value
            with self.assertRaisesRegex(RuntimeError, "historical"):
                devnet.validate_schedule(config, changed, 300)

    def test_upstream_write_is_rejected_before_transport(self):
        with patch.object(devnet, "request_json") as transport:
            for method in ("eth_sendRawTransaction", "eth_sendTransaction", "anvil_setBalance",
                           "optimism_safeHeadAtL1Block", "optimism_rollupConfig"):
                with self.assertRaisesRegex(RuntimeError, "upstream write"):
                    devnet.rpc("https://secret.invalid/key", method, upstream=True)
            transport.assert_not_called()

    def test_provider_failure_does_not_print_secret(self):
        with patch.object(devnet, "request_json", return_value={"error": {"message": "secret-api-key"}}):
            with self.assertRaises(RuntimeError) as caught:
                devnet.rpc("https://secret.invalid/key", "eth_call", upstream=True)
        self.assertNotIn("secret-api-key", str(caught.exception))
        self.assertNotIn("secret.invalid", str(caught.exception))

    def test_durable_operation_is_not_resent_and_rejects_changed_request(self):
        receipt = {"status": "0x1", "blockNumber": "0x65"}
        transaction = {"from": "0x1", "to": "0x2", "data": "0x1234", "value": "0x0"}
        self.fork.manifest["operations"]["bootstrap"] = {"transaction": transaction, "hash": "0xtx"}
        with patch.object(self.fork, "assert_local_l1"), patch.object(devnet, "run", return_value="0x1234"), \
                patch.object(devnet, "rpc", return_value=receipt) as transport:
            self.fork.send("bootstrap", "0x1", "0x2", "set(uint256)", 1)
            self.assertTrue(all(call.args[1] == "eth_getTransactionReceipt" for call in transport.call_args_list))
            with self.assertRaisesRegex(RuntimeError, "different operation"):
                self.fork.send("bootstrap", "0x1", "0x2", "set(uint256)", 1, value=2)
        stored = json.loads((self.fork.directory / "manifest.json").read_text())
        self.assertEqual(stored["operations"]["bootstrap"]["receipt"], receipt)
        self.assertEqual((self.fork.directory / "manifest.json").stat().st_mode & 0o777, 0o600)

    def test_ambiguous_send_is_preserved_not_blindly_retried(self):
        self.fork.manifest["operations"]["bootstrap"] = {
            "transaction": {"from": "0x1", "to": "0x2", "data": "0x1234", "value": "0x0", "nonce": "0x4"}}
        with patch.object(self.fork, "assert_local_l1"), patch.object(devnet, "run", return_value="0x1234"), \
                patch.object(devnet, "rpc") as transport:
            with self.assertRaisesRegex(RuntimeError, "reconcile its nonce"):
                self.fork.send("bootstrap", "0x1", "0x2", "set(uint256)", 1)
            transport.assert_not_called()

    def test_resume_rejects_missing_l1_dump_before_starting_any_database(self):
        self.fork.manifest["phase"] = "stopped"
        self.fork.manifest["datadirs"] = {"sequencer": str(self.datadir("a")), "validator": str(self.datadir("b"))}
        with patch.object(self.fork, "running", return_value=False), patch.object(self.fork, "endpoint"), \
                patch.object(self.fork, "inspect") as inspect:
            with self.assertRaisesRegex(RuntimeError, "L1 state is missing"):
                self.fork.start()
            inspect.assert_not_called()

    def test_inspection_defers_only_unpersisted_checkpoint_tail(self):
        for height, actual, deferred in ((128, None, True), (123, None, False),
                                         (122, None, False), (128, {"hash": "0xwrong"}, False)):
            with self.subTest(height=height, actual=actual):
                self.fork.manifest["last_stop"] = {
                    "validator": {"safe_l2": {"number": height, "hash": "0xexpected"}}}
                with patch.object(self.fork, "compose"), patch.object(self.fork, "await_rpc"), \
                        patch.object(self.fork, "containers", return_value=[]), \
                        patch.object(self.fork, "url", side_effect=lambda role: role), \
                        patch.object(devnet, "run", return_value=json.dumps(snapshot())), \
                        patch.object(devnet, "rpc", return_value=actual):
                    if deferred:
                        self.assertEqual(len(self.fork.inspect()), 2)
                    else:
                        with self.assertRaisesRegex(RuntimeError, "checkpoint"):
                            self.fork.inspect()

    def test_resume_replays_and_matches_checkpoints_before_mining_or_sequencing(self):
        for outcome in ("recovered", "unavailable", "conflict", "stalled"):
            with self.subTest(outcome=outcome):
                self.fork.manifest = manifest()
                self.fork.manifest.update(boundary_validated=True, bootstrapped=True, last_stop={
                    "validator": {"safe_l2": {"number": 129, "hash": "0xsafe"},
                                  "finalized_l2": {"number": 125, "hash": "0xfinal"}}})
                self.fork.timeout = 0.02
                polls, checked, effects = [], set(), []

                def status(role):
                    if role == "validator":
                        polls.append(role)
                        if outcome == "unavailable" and len(polls) == 1:
                            raise devnet.Unavailable("validator consensus RPC restarting")
                        return sync_status(101, safe=129 if len(polls) > 1 and outcome != "stalled" else 123,
                                           unsafe=130)
                    return sync_status(101, safe=130)

                def transport(url, method, *args):
                    if method == "admin_startSequencer":
                        enable("sequence")
                    if method == "eth_getBlockByNumber":
                        if url == "validator" and args[0] != "latest":
                            height = devnet.number(args[0])
                            checked.add(height)
                            return {"hash": "0xwrong" if outcome == "conflict" else
                                    {129: "0xsafe", 125: "0xfinal"}[height]}
                        return {"number": "0x82", "hash": "0xhead"}
                    return method != "admin_sequencerActive"

                def enable(effect):
                    self.assertGreaterEqual(len(polls), 2, "must await safe derivation, not just EL availability")
                    self.assertEqual(checked, {125, 129}, "must verify both saved hashes before enabling writes")
                    effects.append(effect)

                with patch.object(devnet, "validate_paths"), \
                        patch.multiple(self.fork, endpoint=DEFAULT, await_rpc=DEFAULT, assert_local_l1=DEFAULT,
                                       validate_restored_contracts=DEFAULT, wait_upgrades=DEFAULT,
                                       peers=DEFAULT, start_batcher=DEFAULT, schedule_denim=DEFAULT), \
                        patch.object(self.fork, "running", return_value=False), \
                        patch.object(self.fork, "compose"), \
                        patch.object(self.fork, "inspect", return_value=[]), \
                        patch.object(self.fork, "running_services", return_value=devnet.FORK_SERVICES), \
                        patch.object(self.fork, "consensus_ready", return_value=True), \
                        patch.object(self.fork, "mine", side_effect=lambda: enable("mine")), \
                        patch.object(self.fork, "url", side_effect=lambda role: role), \
                        patch.object(self.fork, "sync_status", side_effect=status), \
                        patch.object(devnet.time, "time", return_value=1234), \
                        patch.object(devnet.time, "sleep"), \
                        patch.object(devnet, "rpc", side_effect=transport), patch("builtins.print"):
                    if outcome in ("recovered", "unavailable"):
                        self.fork.start()
                        self.assertEqual(effects, ["mine", "sequence"])
                    else:
                        with self.assertRaises(RuntimeError):
                            self.fork.start()
                        self.assertEqual(effects, [])
                        self.assertNotEqual(self.fork.manifest["phase"], "running")

    def test_start_checks_batcher_rpc_before_sequencing_and_rejects_exited_batcher(self):
        for missing_miner in (True, False):
            with self.subTest(missing_miner=missing_miner):
                self.fork.manifest = manifest()
                self.fork.manifest.update(boundary_validated=True, bootstrapped=True)
                self.fork.manifest["operations"] = {
                    name: {"receipt": {"blockNumber": "0x13"}}
                    for name in ("set-batcher", "set-signer")
                }
                calls = []
                wait = devnet.wait

                def waiting(description, *args, **kwargs):
                    if description.startswith("sequencer catch-up"):
                        calls.append("catch-up")
                    return wait(description, *args, **kwargs)

                def transport(url, method, *args):
                    calls.append(method)
                    if method.startswith("miner_"):
                        if missing_miner:
                            raise devnet.Unavailable("miner method unavailable")
                        return ["0x0", "0x0"] if method == "miner_getMaxDASize" else True
                    if method == "admin_sequencerActive":
                        return False
                    if method == "eth_getBlockByNumber":
                        return {"number": "0x7b", "hash": "0x123"}
                    return True

                with patch.object(devnet, "validate_paths"), \
                        patch.object(self.fork, "running", return_value=False), \
                        patch.object(self.fork, "endpoint"), \
                        patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)), \
                        patch.object(self.fork, "await_rpc"), \
                        patch.object(self.fork, "assert_local_l1"), \
                        patch.object(self.fork, "validate_restored_contracts"), \
                        patch.object(self.fork, "inspect", return_value=[]), \
                        patch.object(self.fork, "running_services", return_value={"l1", "sequencer", "validator"}), \
                        patch.object(self.fork, "consensus_ready", return_value=True), \
                        patch.object(self.fork, "wait_upgrades"), \
                        patch.object(self.fork, "mine"), \
                        patch.object(self.fork, "peers"), \
                        patch.object(self.fork, "url", side_effect=lambda role: role), \
                        patch.object(self.fork, "sync_status", return_value=sync_status(101)), \
                        patch.object(devnet.time, "time", return_value=1234), \
                        patch.object(devnet, "wait", side_effect=waiting), \
                        patch.object(devnet, "rpc", side_effect=transport):
                    with self.assertRaisesRegex(RuntimeError, "miner" if missing_miner else "batcher"):
                        self.fork.start()
                self.assertNotEqual(devnet.SnapshotFork(self.fork.directory).manifest["phase"], "running")
                self.assertEqual(calls[-2:], [("stop", "batcher", "sequencer", "validator"), ("stop", "l1")])
                # A broken batcher must fail before potentially hours of unbatched catch-up.
                self.assertNotIn("catch-up", calls)
                if missing_miner:
                    self.assertNotIn("admin_startSequencer", calls)

    def test_start_batches_during_wall_time_catch_up_and_requires_live_batcher_until_running(self):
        for batcher_exits in (False, True):
            with self.subTest(batcher_exits=batcher_exits):
                self.fork.manifest = manifest()
                self.fork.manifest.update(boundary_validated=True, bootstrapped=True)
                calls, polls = [], []
                wait = devnet.wait

                def waiting(description, *args, **kwargs):
                    if description.startswith("sequencer catch-up"):
                        calls.append("catch-up")
                    return wait(description, *args, **kwargs)

                def status(role):
                    # An hour behind wall time with an old L1 origin, then converged.
                    current = sync_status(101)
                    current["unsafe_l2"]["timestamp"] = 1234 - (3600 if not polls else 0)
                    polls.append(role)
                    return current

                def transport(url, method, *args):
                    calls.append(method)
                    if method == "eth_getBlockByNumber":
                        return {"number": "0x7b", "hash": "0x123"}
                    return method != "admin_sequencerActive"

                with patch.object(devnet, "validate_paths"), \
                        patch.multiple(self.fork, endpoint=DEFAULT, await_rpc=DEFAULT, assert_local_l1=DEFAULT,
                                       validate_restored_contracts=DEFAULT, wait_upgrades=DEFAULT,
                                       mine=DEFAULT, peers=DEFAULT), \
                        patch.object(self.fork, "schedule_denim", side_effect=lambda: calls.append("denim")), \
                        patch.object(self.fork, "running", return_value=False), \
                        patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)), \
                        patch.object(self.fork, "inspect", return_value=[]), \
                        patch.object(self.fork, "running_services", side_effect=lambda: {
                            "l1", "sequencer", "validator"} | (set() if batcher_exits and polls else {"batcher"})), \
                        patch.object(self.fork, "consensus_ready", return_value=True), \
                        patch.object(self.fork, "start_batcher", side_effect=lambda: calls.append("batched")), \
                        patch.object(self.fork, "url", side_effect=lambda role: role), \
                        patch.object(self.fork, "sync_status", side_effect=status), \
                        patch.object(devnet.time, "time", return_value=1234), \
                        patch.object(devnet.time, "sleep"), \
                        patch.object(devnet, "wait", side_effect=waiting), \
                        patch.object(devnet, "rpc", side_effect=transport), \
                        patch("builtins.print"):
                    if batcher_exits:
                        with self.assertRaisesRegex(RuntimeError, "batcher exited"):
                            self.fork.start()
                    else:
                        self.fork.start()
                phase = devnet.SnapshotFork(self.fork.directory).manifest["phase"]
                self.assertEqual(phase == "running", not batcher_exits)
                # Old-origin blocks are batched while catching up, not after.
                self.assertLess(calls.index("admin_startSequencer"), calls.index("batched"))
                self.assertLess(calls.index("batched"), calls.index("catch-up"))
                if batcher_exits:
                    self.assertEqual(calls[-2:], [("stop", "batcher", "sequencer", "validator"), ("stop", "l1")])
                    self.assertNotIn("denim", calls)
                else:
                    self.assertIn("denim", calls, "up must schedule Denim without a separate manual step")
                    self.assertLess(calls.index("catch-up"), calls.index("denim"))

    def test_status_reports_missing_batcher_as_degraded_even_after_successful_start(self):
        self.fork.manifest["phase"] = "running"
        records = [container(role) for role in ("l1", "sequencer", "validator")]
        records.append(container("batcher", running=False))
        with patch.object(self.fork, "containers", return_value=records):
            self.assertEqual(self.fork.status()["phase"], "degraded")

    def test_status_reports_running_only_with_every_fork_service_running(self):
        self.fork.manifest["phase"] = "running"
        records = [container(role) for role in ("l1", "sequencer", "validator", "batcher")]
        with patch.object(self.fork, "containers", return_value=records), \
                patch.object(devnet, "rpc") as transport:
            self.assertEqual(self.fork.status()["phase"], "running")
            transport.assert_not_called()

    def batching(self, safe_heads, alive, hashes):
        """Patches start_batcher's collaborators: per-poll safe heads and batcher liveness."""
        polls = iter(safe_heads)
        liveness = iter(alive)
        current = {}
        calls = []

        def status(role):
            if role == "sequencer":
                current.update(zip(devnet.ROLES, next(polls)))
            return sync_status(101, safe=current[role], safe_hash=hex(current[role]))

        def node(url, method, *args):
            calls.append((url, method, *args))
            return {"hash": hashes[url]}
        self.addCleanup(patch.stopall)
        patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)).start()
        patch.object(self.fork, "running_services",
                     side_effect=lambda: {"l1", *devnet.ROLES} | ({"batcher"} if next(liveness) else set())).start()
        patch.object(self.fork, "sync_status", side_effect=status).start()
        patch.object(self.fork, "url", side_effect=lambda role: role).start()
        patch.object(devnet, "rpc", side_effect=node).start()
        patch.object(devnet.time, "sleep").start()
        return calls

    def test_batcher_start_waits_for_both_safe_heads_and_checks_common_canonical_hash(self):
        calls = self.batching([(123, 123), (123, 123), (125, 123), (126, 124)], [True] * 3,
                              {"sequencer": "0xsame", "validator": "0xsame"})
        self.fork.start_batcher()
        self.assertEqual(calls[0], ("up", "-d", "--no-build", "batcher"))
        self.assertEqual(calls[1:], [(role, "eth_getBlockByNumber", hex(124), False) for role in devnet.ROLES])

    def test_batcher_start_rejects_divergent_safe_block(self):
        self.batching([(123, 123), (124, 124)], [True], {"sequencer": "0xa", "validator": "0xb"})
        with self.assertRaisesRegex(RuntimeError, "disagree on canonical safe block 124"):
            self.fork.start_batcher()

    def test_batcher_start_fails_when_batcher_exits_cleanly_while_waiting(self):
        self.batching([(123, 123), (123, 123)], [True, False], {})
        with self.assertRaisesRegex(RuntimeError, "batcher exited.*code 0"):
            self.fork.start_batcher()

    def test_restored_schedule_mismatch_does_not_bootstrap_over_it(self):
        self.fork.manifest["schedule"] = [100] * 13
        self.fork.manifest["denim_timestamp"] = 9999
        with patch.object(devnet, "call", return_value=[100] * 13 + [9998]):
            with self.assertRaisesRegex(RuntimeError, "schedule differs"):
                self.fork.validate_restored_contracts()

    def test_resume_reconciles_mined_upgrade_without_sending_it_again(self):
        self.fork.manifest.update(schedule=[100] * 13, pending_denim_timestamp=9000)
        self.fork.manifest["operations"]["schedule-denim"] = {"hash": "0xtx"}
        with patch.object(devnet, "call", return_value=[100] * 13 + [9000]), \
                patch.object(devnet, "rpc", return_value={"status": "0x1"}) as transport:
            self.fork.validate_restored_contracts()
            transport.assert_called_once_with(self.fork.url("l1"), "eth_getTransactionReceipt", "0xtx")
        restored = devnet.SnapshotFork(self.fork.directory)
        self.assertEqual(restored.manifest["denim_timestamp"], 9000)
        self.assertNotIn("pending_denim_timestamp", restored.manifest)

    @contextlib.contextmanager
    def protocol_versions(self, schedule, notice=3600, l1_time=1290, l2_time=1295, now=1293.4,
                          cobalt=800, minimum_after="42"):
        """A running fork whose fake ProtocolVersions applies sent Denim writes like the real contract."""
        self.fork.manifest.update(phase="running", schedule=[100] * 12 + [cobalt])
        contract = {"schedule": [100] * 12 + [cobalt] + list(schedule), "minimum": "42"}
        sent, printed, receipts = [], [], []

        def read(url, address, signature, *args, **_):
            return {"getSchedule()(uint64[])": lambda: list(map(str, contract["schedule"])),
                    "MIN_NOTICE()(uint64)": lambda: str(notice),
                    "minimumProtocolVersion()(uint256)": lambda: contract["minimum"],
                    "proxyAdminOwner()(address)": lambda: "0x" + "a" * 40}[signature]()

        def send(name, sender, target, signature, *args):
            sent.append((signature, *args))
            self.fork.manifest["operations"].setdefault(name, {"hash": "0xsent"})
            if signature.startswith("registerUpgrade"):
                contract["schedule"].append(args[0])
            else:
                contract["schedule"][args[0]] = args[1]
            contract["minimum"] = minimum_after

        def node(url, method, *args):
            if method == "eth_getTransactionReceipt":
                receipts.append(args[0])
                return {"status": "0x1"}
            return {"timestamp": hex(l1_time if url == "l1" else l2_time)}

        with patch.object(self.fork, "assert_local_l1"), patch.object(self.fork, "wait_upgrades") as observed, \
                patch.object(self.fork, "url", side_effect=lambda role: role), \
                patch.object(self.fork, "send", side_effect=send), \
                patch.object(devnet, "call", side_effect=read), patch.object(devnet, "rpc", side_effect=node), \
                patch.object(devnet.time, "time", return_value=now), \
                patch("builtins.print", side_effect=lambda *args, **_: printed.append(" ".join(map(str, args)))):
            yield {"sent": sent, "printed": printed, "receipts": receipts, "observed": observed}

    def test_denim_defaults_to_earliest_even_timestamp_from_live_notice_and_latest_clock(self):
        # max(L1, L2, wall) + MIN_NOTICE + slot is the contract floor; one more slot, rounded up to even.
        for notice, now, cobalt, expected in ((3600, 1293.4, 800, 4920), (7200, 1293.4, 800, 8520),
                                              (3600, 1401, 800, 5026), (3600, 1293.4, 9001, 9002)):
            with self.subTest(notice=notice, now=now, cobalt=cobalt):
                self.fork.manifest = manifest()
                with self.protocol_versions([], notice=notice, now=now, cobalt=cobalt) as fake:
                    self.fork.schedule_denim()
                self.assertEqual(fake["sent"], [("registerUpgrade(uint64,uint256)", expected, 0)])
                fake["observed"].assert_called_once_with()
                stored = devnet.SnapshotFork(self.fork.directory).manifest
                self.assertEqual(stored["denim_timestamp"], expected)
                self.assertNotIn("pending_denim_timestamp", stored)
                self.assertNotIn("baseline_verified", stored)
                output = "\n".join(fake["printed"])
                self.assertIn(f"{notice}s notice", output)
                self.assertIn(f"Activation in {expected - int(now)}s", output)
                self.assertNotIn("://", output)

    def test_denim_explicit_timestamp_keeps_notice_grid_and_cobalt_validations(self):
        # Contract floor: max(1290, 1295, 1293) + 3600 + 12 = 4907.
        for timestamp, schedule, cobalt, error in (
            (4906, [], 800, "3600s notice"), (4909, [], 800, "two-second grid"),
            (4908, [], 0, "Cobalt must already be scheduled"), (4908, [], 5000, "Cobalt must already be scheduled"),
            (4908, [], 800, None), (4908, [0], 800, None),
        ):
            with self.subTest(timestamp=timestamp, schedule=schedule, cobalt=cobalt):
                self.fork.manifest = manifest()
                with self.protocol_versions(schedule, cobalt=cobalt) as fake:
                    if error:
                        with self.assertRaisesRegex(RuntimeError, error):
                            self.fork.schedule_denim(timestamp)
                        self.assertEqual(fake["sent"], [])
                        self.assertNotIn("pending_denim_timestamp", self.fork.manifest)
                    else:
                        self.fork.schedule_denim(timestamp)
                        self.assertEqual(fake["sent"], [("setTimestamp(uint256,uint64)", 13, 4908) if schedule
                                                        else ("registerUpgrade(uint64,uint256)", 4908, 0)])
                        self.assertEqual(self.fork.manifest["denim_timestamp"], 4908)

    def test_denim_write_must_retain_the_minimum_protocol_version(self):
        with self.protocol_versions([], minimum_after="43"):
            with self.assertRaisesRegex(RuntimeError, "minimum protocol version"):
                self.fork.schedule_denim()
        self.assertNotIn("denim_timestamp", self.fork.manifest)

    def test_denim_preserves_scheduled_active_or_external_schedule_without_writes(self):
        for name, recorded, schedule, timestamp, outcome in (
            ("scheduled", 4920, [4920], None, "Activation in 3627s"),
            ("same explicit", 4920, [4920], 4920, "Activation in 3627s"),
            ("active", 1200, [1200], None, "passed 93s ago"),
            ("external", None, [4920], None, "Activation in 3627s"),
            ("move", 4920, [4920], 5000, "refusing to move"),
            ("move active", 1200, [1200], 4920, "refusing to move"),
            ("lost", 4920, [], None, "differs from manifest"),
        ):
            with self.subTest(name):
                self.fork.manifest = manifest()
                if recorded:
                    self.fork.manifest["denim_timestamp"] = recorded
                with self.protocol_versions(schedule) as fake:
                    if outcome.startswith(("Activation", "passed")):
                        self.fork.schedule_denim(timestamp)
                        self.assertIn(outcome, "\n".join(fake["printed"]))
                        fake["observed"].assert_called_once_with()
                        self.assertEqual(self.fork.manifest["denim_timestamp"], schedule[0])
                    else:
                        with self.assertRaisesRegex(RuntimeError, outcome):
                            self.fork.schedule_denim(timestamp)
                    self.assertEqual(fake["sent"], [])

    def test_denim_resumes_journaled_submission_without_duplicate_writes(self):
        for name, operation, schedule, timestamp, sent in (
            ("mined", {"hash": "0xtx"}, [4920], None, []),
            ("unmined", {"hash": "0xtx"}, [], None, [("registerUpgrade(uint64,uint256)", 4920, 0)]),
            ("never sent", None, [], None, [("registerUpgrade(uint64,uint256)", 4920, 0)]),
            ("different", {"hash": "0xtx"}, [], 5000, "already pending at 4000"),
        ):
            with self.subTest(name):
                self.fork.manifest = manifest()
                # A never-sent journal holds a stale timestamp; the others already submitted 4920.
                self.fork.manifest["pending_denim_timestamp"] = 4920 if operation and timestamp is None else 4000
                if operation:
                    self.fork.manifest["operations"]["schedule-denim"] = operation
                with self.protocol_versions(schedule) as fake:
                    if isinstance(sent, str):
                        with self.assertRaisesRegex(RuntimeError, sent):
                            self.fork.schedule_denim(timestamp)
                        self.assertEqual(fake["sent"], [])
                        continue
                    self.fork.schedule_denim(timestamp)
                # send() itself only awaits the receipt of an operation that records a hash.
                self.assertEqual(fake["sent"], sent)
                self.assertEqual(fake["receipts"], ["0xtx" if operation else "0xsent"])
                stored = devnet.SnapshotFork(self.fork.directory).manifest
                self.assertEqual(stored["denim_timestamp"], 4920)
                self.assertNotIn("pending_denim_timestamp", stored)

    def test_status_does_not_claim_activation_from_wall_clock_without_rpc(self):
        for fields, expected in (
            ({}, {"state": "unscheduled"}),
            ({"pending_denim_timestamp": 4920}, {"state": "submission pending", "pending_timestamp": 4920}),
            ({"denim_timestamp": 4920}, {"state": "scheduled", "timestamp": 4920, "utc": "1970-01-01T01:22:00Z",
                                         "seconds_until_activation": 3627}),
            ({"denim_timestamp": 1200}, {"state": "activation time reached", "timestamp": 1200, "utc": "1970-01-01T00:20:00Z",
                                         "seconds_until_activation": 0}),
        ):
            with self.subTest(fields=fields):
                self.fork.manifest = {**manifest(), **fields}
                with patch.object(self.fork, "containers", return_value=[]), \
                        patch.object(devnet.time, "time", return_value=1293.4), \
                        patch.object(devnet, "rpc") as transport:
                    self.assertEqual(self.fork.status()["denim"], expected)
                    transport.assert_not_called()

    def test_schedule_denim_command_timestamp_is_optional(self):
        self.fork.save()
        for args, expected in (([], None), (["4920"], 4920)):
            with self.subTest(args=args), \
                    patch.object(sys, "argv", ["launcher", "schedule-denim", "--dir", str(self.fork.directory), *args]), \
                    patch.object(devnet.SnapshotFork, "schedule_denim") as schedule:
                devnet.main()
                schedule.assert_called_once_with(expected)

    def test_denim_scheduling_failure_after_start_keeps_the_running_fork(self):
        self.fork.manifest.update(boundary_validated=True, bootstrapped=True)
        calls = []

        def transport(url, method, *args):
            if method == "eth_getBlockByNumber":
                return {"number": "0x7b", "hash": "0x123"}
            return method != "admin_sequencerActive"

        with patch.object(devnet, "validate_paths"), \
                patch.multiple(self.fork, endpoint=DEFAULT, await_rpc=DEFAULT, assert_local_l1=DEFAULT,
                               validate_restored_contracts=DEFAULT, inspect=DEFAULT, wait_upgrades=DEFAULT,
                               wait_checkpoints=DEFAULT, mine=DEFAULT, peers=DEFAULT, start_batcher=DEFAULT), \
                patch.object(self.fork, "schedule_denim", side_effect=RuntimeError("Denim receipt timed out")), \
                patch.object(self.fork, "running", return_value=False), \
                patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)), \
                patch.object(self.fork, "running_services", return_value=devnet.FORK_SERVICES), \
                patch.object(self.fork, "consensus_ready", return_value=True), \
                patch.object(self.fork, "url", side_effect=lambda role: role), \
                patch.object(self.fork, "sync_status", return_value=sync_status(101)), \
                patch.object(devnet.time, "time", return_value=1234), \
                patch.object(devnet, "rpc", side_effect=transport), patch("builtins.print"):
            with self.assertRaisesRegex(RuntimeError, "running but Denim.*receipt timed out.*rerun schedule-denim"):
                self.fork.start()
        self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["phase"], "running")
        self.assertFalse([call for call in calls if call[0] == "stop"])

    def test_inspection_uses_persisted_denim_and_leaves_original_config_unchanged(self):
        config = {"genesis": {"l2_time": 100}, "base": {"cobalt": 1000}}
        (self.fork.directory / "config").mkdir()
        devnet.write_json(self.fork.directory / "config/rollup.json", config)
        self.fork.manifest.update(initial=[{"rollup_config": config}], schedule=[100] * 13, denim_timestamp=9000)
        with patch.object(self.fork, "compose"), patch.object(self.fork, "await_rpc"), \
                patch.object(self.fork, "containers", return_value=[]), \
                patch.object(self.fork, "url", return_value="http://10.9.0.2:8545"), \
                patch.object(devnet, "run", return_value="{}") as inspect:
            self.fork.inspect()
            self.assertIn("--rollup-config", inspect.call_args.args)
            self.assertNotIn("--find-fork", inspect.call_args.args)
        self.assertEqual(json.loads((self.fork.directory / "config/inspection.json").read_text())["base"]["denim"], 9000)
        self.assertEqual(json.loads((self.fork.directory / "config/rollup.json").read_text()), config)

    def test_stop_orders_dependents_before_l1_and_never_removes_data(self):
        calls = []
        with patch.object(self.fork, "running", return_value=True), \
                patch.object(self.fork, "containers", return_value=[container("sequencer")]), \
                patch.object(self.fork, "consensus_ready", return_value=True), \
                patch.object(self.fork, "sync_status", return_value={}), \
                patch.object(devnet, "rpc", side_effect=lambda _, method, *args: calls.append(method) or True), \
                patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)):
            self.fork.stop()
        self.assertLess(calls.index("admin_stopSequencer"), calls.index(("stop", "batcher")))
        self.assertEqual(calls[-1], ("stop", "l1"))
        self.assertTrue(all(call[0] == "stop" for call in calls if isinstance(call, tuple)))

    def test_stop_with_stopped_sequencer_still_stops_everything_and_keeps_validator_status(self):
        self.fork.manifest["phase"] = "running"
        calls = []
        with patch.object(self.fork, "running", return_value=True), \
                patch.object(self.fork, "containers", return_value=[container("validator")]), \
                patch.object(devnet, "rpc", side_effect=lambda url, method, *args: sync_status(101)), \
                patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)):
            self.fork.stop()
        self.assertEqual(calls[-1], ("stop", "l1"))
        stored = devnet.SnapshotFork(self.fork.directory).manifest
        self.assertEqual(set(stored["last_stop"]), {"validator"})
        self.assertEqual(stored["phase"], "stopped")

    def test_stop_with_ambiguous_nodes_stops_all_containers_then_reports_the_failure(self):
        calls = []
        both = [container("sequencer", "10.9.0.2"), container("inspect-sequencer", "10.9.0.3")]
        with patch.object(self.fork, "running", return_value=True), \
                patch.object(self.fork, "containers", return_value=both), \
                patch.object(devnet, "rpc", return_value=True), \
                patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)):
            with self.assertRaisesRegex(RuntimeError, "ambiguous"):
                self.fork.stop()
        self.assertIn(("stop", "sequencer", "validator", "inspect-sequencer", "inspect-validator"), calls)
        self.assertEqual(calls[-1], ("stop", "l1"))

    def test_stop_before_first_start_keeps_prepared_fork_startable(self):
        with patch.object(self.fork, "running", return_value=False):
            self.fork.stop()
        self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["phase"], "prepared")

    def test_l1_url_uses_published_loopback_port_without_docker_lookup(self):
        with patch.object(devnet, "run") as run:
            self.assertEqual(self.fork.url("l1"), "http://127.0.0.1:19545")
            run.assert_not_called()

    def test_status_omits_secret_command_arguments_and_works_while_start_holds_lock(self):
        self.fork.save()
        record = container("validator", running=False)
        record["Config"]["Cmd"] = ["--private-key=secret", "--fork-url=https://secret.invalid/key"]
        record["State"]["ExitCode"] = 1
        with patch.object(devnet.SnapshotFork, "containers", return_value=[record]), \
                patch.object(devnet.fcntl, "flock", side_effect=BlockingIOError("start holds lock")), \
                patch.object(sys, "argv", ["launcher", "status", "--dir", str(self.fork.directory)]), \
                patch("builtins.print") as output:
            devnet.main()
        serialized = output.call_args.args[0]
        self.assertNotIn("secret", serialized)
        self.assertEqual(json.loads(serialized)["services"],
                         [{"service": "validator", "running": False, "exit_code": 1}])

    def test_execution_url_selects_running_inspection_or_production_container(self):
        for service in ("inspect-sequencer", "sequencer"):
            with patch.object(self.fork, "containers",
                              return_value=[container(service, "10.9.0.7"), container("validator", "10.9.0.8")]):
                self.assertEqual(self.fork.url("sequencer"), "http://10.9.0.7:8545")
                self.assertEqual(self.fork.url("validator"), "http://10.9.0.8:8545")
        with patch.object(self.fork, "containers", return_value=[container("sequencer", "10.9.0.7")]):
            self.assertEqual(self.fork.url("sequencer-cl"), "http://10.9.0.7:9545")

    def test_unavailable_role_is_retryable_but_ambiguous_identity_is_not(self):
        cases = {
            "stopped": ([container("sequencer", running=False)], "sequencer", devnet.Unavailable),
            "inspection has no consensus RPC": ([container("inspect-sequencer")], "sequencer-cl", devnet.Unavailable),
            "other project": ([container("sequencer", project="snapshot-other")], "sequencer", devnet.Unavailable),
            "inspection and production": ([container("sequencer"), container("inspect-sequencer", "10.9.0.3")],
                                          "sequencer", RuntimeError),
            "duplicate service": ([container("validator"), container("validator", "10.9.0.3")],
                                  "validator-cl", RuntimeError),
        }
        for name, (containers, role, error) in cases.items():
            with self.subTest(name), patch.object(self.fork, "containers", return_value=containers):
                with self.assertRaises(error) as caught:
                    self.fork.url(role)
                if error is RuntimeError:
                    self.assertNotIsInstance(caught.exception, devnet.Unavailable)
                    with self.assertRaisesRegex(RuntimeError, "ambiguous"):
                        self.fork.consensus_ready(role.removesuffix("-cl"))
                else:
                    self.assertFalse(self.fork.consensus_ready("sequencer"))

    def test_l2_rpc_resolves_only_through_the_internal_network(self):
        cases = (
            {"snapshot-fixture_private": {"IPAddress": "10.9.0.2"}, "bridge": {"IPAddress": "172.17.0.2"}},
            {"bridge": {"IPAddress": "172.17.0.2"}},
            {"snapshot-fixture_private": {"IPAddress": ""}},
        )
        for networks in cases:
            with self.subTest(networks=networks), \
                    patch.object(self.fork, "containers", return_value=[container("sequencer", networks=networks)]):
                with self.assertRaisesRegex(RuntimeError, "internal network"):
                    self.fork.url("sequencer")

    def test_container_lookup_is_project_scoped_and_refreshed_by_lifecycle_commands(self):
        commands = []
        def docker(*args, **_):
            commands.append(args)
            return "abc" if args[1] == "ps" else json.dumps([container("sequencer")])
        with patch.object(devnet, "run", side_effect=docker), patch.object(self.fork, "compose_env", return_value={}):
            self.fork.url("sequencer")
            self.fork.url("sequencer-cl")
            self.assertEqual(len(commands), 2)
            self.assertIn("label=com.docker.compose.project=snapshot-fixture", commands[0])
            self.fork.compose("stop", "sequencer")
            self.fork.url("sequencer")
            self.assertEqual(len(commands), 5)

    def test_gossip_connects_the_private_ip_of_the_other_node(self):
        containers = [container("sequencer", "10.9.0.2"), container("validator", "10.9.0.3")]
        calls = []
        def node(url, method, *args):
            calls.append((url, method, *args))
            return {"peerID": "seq" if "10.9.0.2" in url else "val"}
        with patch.object(self.fork, "containers", return_value=containers), patch.object(devnet, "rpc", side_effect=node):
            self.fork.peers()
        self.assertIn(("http://10.9.0.2:9545", "opp2p_connectPeer", "/ip4/10.9.0.3/tcp/9222/p2p/val"), calls)
        self.assertIn(("http://10.9.0.3:9545", "opp2p_connectPeer", "/ip4/10.9.0.2/tcp/9222/p2p/seq"), calls)

    def test_init_discovers_fork_only_on_sequencer_with_normalized_credentials(self):
        self.fork.manifest["upstreams"] = {"execution": "CUSTOM_EXECUTION", "beacon": "CUSTOM_BEACON"}
        calls = []
        def inspector(*args, env=None, timeout=None, secrets=None):
            calls.append((args, env, timeout, secrets))
            result = snapshot()
            if "--find-fork" in args:
                result["fork"] = {"number": 100, "hash": "0xf", "timestamp": 1240, "parentHash": "0xe"}
            return json.dumps(result)
        environment = {"CUSTOM_EXECUTION": "https://execution.invalid/key", "CUSTOM_BEACON": "https://beacon.invalid/key",
                       "SNAPSHOT_UPSTREAM_EXECUTION": "https://wrong.invalid"}
        with patch.dict(os.environ, environment), patch.object(self.fork, "compose"), \
                patch.object(self.fork, "await_rpc"), patch.object(self.fork, "containers", return_value=[]), \
                patch.object(self.fork, "url", side_effect=lambda role: f"http://{role}.private:8545"), \
                patch.object(devnet, "run", side_effect=inspector):
            result = self.fork.inspect(discover=True)
        (sequencer, sequencer_env, timeout, secrets), (validator, validator_env, _, _) = calls
        self.assertEqual(sequencer[sequencer.index("--rpc-url") + 1], "http://sequencer.private:8545")
        self.assertEqual(sequencer[sequencer.index("--timeout") + 1], str(self.fork.timeout))
        self.assertIn("--find-fork", sequencer)
        self.assertGreater(timeout, self.fork.timeout)
        self.assertEqual(sequencer_env["SNAPSHOT_UPSTREAM_EXECUTION"], "https://execution.invalid/key")
        self.assertEqual(sequencer_env["SNAPSHOT_UPSTREAM_BEACON"], "https://beacon.invalid/key")
        self.assertIn("https://execution.invalid/key", secrets)
        self.assertNotIn("--find-fork", validator)
        self.assertNotIn("SNAPSHOT_UPSTREAM_EXECUTION", validator_env or {})
        self.assertEqual(result[0]["fork"]["number"], 100)
        self.assertNotIn("fork", result[1])

    def test_discovery_without_fork_output_fails(self):
        with patch.dict(os.environ, {"TEST_EXECUTION_URL": "https://e.invalid", "TEST_BEACON_URL": "https://b.invalid"}), \
                patch.object(self.fork, "compose"), patch.object(self.fork, "await_rpc"), \
                patch.object(self.fork, "containers", return_value=[]), \
                patch.object(self.fork, "url", return_value="http://10.9.0.2:8545"), \
                patch.object(devnet, "run", return_value=json.dumps(snapshot())):
            with self.assertRaisesRegex(RuntimeError, "fork"):
                self.fork.inspect(discover=True)

    def test_inspection_refuses_datadirs_held_by_production_nodes(self):
        with patch.object(self.fork, "containers", return_value=[container("validator")]), \
                patch.object(self.fork, "compose") as compose:
            with self.assertRaisesRegex(RuntimeError, "production"):
                self.fork.inspect()
            compose.assert_not_called()

    def test_inspector_failure_is_actionable_without_endpoint_credentials(self):
        script = ("import sys; sys.stderr.write('history unavailable from https://user:pw@rpc.invalid/v2/k "
                  "and token-123'); sys.exit(1)")
        with self.assertRaises(RuntimeError) as caught:
            devnet.run(sys.executable, "-c", script, secrets=("token-123",))
        self.assertIn("history unavailable", str(caught.exception))
        for secret in ("rpc.invalid", "token-123", "pw@"):
            self.assertNotIn(secret, str(caught.exception))

    def test_compose_before_discovery_uses_inert_placeholders_and_cannot_start_the_fork(self):
        del self.fork.manifest["fork"], self.fork.manifest["slot_seconds"]
        devnet.write_json(self.fork.directory / "keys.json", {"signer": "0x1", "batcher": "0x2"})
        environment = self.fork.compose_env()
        self.assertFalse(environment["SNAPSHOT_FORK_BLOCK"].isdigit())
        self.assertFalse(environment["SNAPSHOT_SLOT_SECONDS"].isdigit())
        with patch.object(devnet, "run") as run:
            self.fork.compose("--profile", "inspect", "up", "-d", "--no-build", "inspect-sequencer", "inspect-validator")
            for service in ("l1", "sequencer", "validator", "batcher"):
                with self.assertRaisesRegex(RuntimeError, "before F"):
                    self.fork.compose("up", "-d", "--no-build", service)
            self.assertEqual(run.call_count, 1)

    def test_boundary_waits_until_both_nodes_leave_fork_block(self):
        initial = snapshot()["latest"]["block_info"]
        for statuses in (
            {"sequencer": sync_status(100), "validator": sync_status(100)},
            {"sequencer": sync_status(101), "validator": sync_status(100)},
            {"sequencer": sync_status(99), "validator": sync_status(101)},
        ):
            self.assertIsNone(devnet.derived_boundary(statuses, 100, initial))
        ready = {"sequencer": sync_status(101, safe=124, safe_hash="0x124"),
                 "validator": sync_status(102, safe=124, safe_hash="0x124")}
        self.assertEqual(devnet.derived_boundary(ready, 100, initial), {"number": 124, "hash": "0x124"})

    def test_boundary_rejects_unsafe_tail_divergent_or_short_safe_heads(self):
        initial = snapshot()["latest"]["block_info"]
        for name, statuses in {
            "unsafe": {"sequencer": sync_status(101, unsafe=124, unsafe_hash="0x124"), "validator": sync_status(101)},
            "different": {"sequencer": sync_status(101), "validator": sync_status(101, safe_hash="0xother")},
            "below": {"sequencer": sync_status(101, safe=122, safe_hash="0x122"),
                      "validator": sync_status(101, safe=122, safe_hash="0x122")},
        }.items():
            with self.subTest(name), self.assertRaises(RuntimeError):
                devnet.derived_boundary(statuses, 100, initial)

    def boundary_gate(self, l1_head, canonical="0x123"):
        self.fork.manifest["initial"] = [snapshot()]
        statuses = iter([{"sequencer": sync_status(100), "validator": sync_status(100)},
                         {"sequencer": sync_status(101), "validator": sync_status(100)},
                         {"sequencer": sync_status(101), "validator": sync_status(101)}])
        current = {}
        def status(role):
            if role == "sequencer":
                current.update(next(statuses))
            return current[role]
        def node(url, method, *args):
            if method == "eth_blockNumber":
                return hex(l1_head)
            if url == "http://l1":
                self.assertEqual(args[0], hex(101))
                return {"number": hex(101), "hash": "0xsuccessor", "parentHash": "0xf"}
            self.assertEqual(args[0], hex(123))
            return {"hash": canonical}
        mine = patch.object(self.fork, "mine").start()
        self.addCleanup(patch.stopall)
        patch.object(self.fork, "sync_status", side_effect=status).start()
        patch.object(self.fork, "url", side_effect=lambda role: "http://" + role).start()
        patch.object(devnet, "rpc", side_effect=node).start()
        patch.object(devnet.time, "sleep").start()
        return mine

    def test_boundary_gate_mines_one_successor_and_persists_the_derived_boundary(self):
        mine = self.boundary_gate(100)
        self.fork.wait_boundary()
        mine.assert_called_once()
        stored = devnet.SnapshotFork(self.fork.directory).manifest
        self.assertTrue(stored["boundary_validated"])
        self.assertEqual(stored["boundary"]["safe_l2"], {"number": 123, "hash": "0x123"})
        self.assertEqual(stored["boundary"]["current_l1"]["validator"]["number"], 101)
        self.assertEqual(stored["boundary"]["l1_successor"], {"number": "0x65", "hash": "0xsuccessor"})

    def test_boundary_gate_resume_does_not_mine_a_second_successor(self):
        mine = self.boundary_gate(101)
        self.fork.wait_boundary()
        mine.assert_not_called()

    def test_boundary_gate_rejects_reorged_snapshot_head_without_persisting(self):
        self.boundary_gate(100, canonical="0xother")
        with self.assertRaisesRegex(RuntimeError, "no longer canonical"):
            self.fork.wait_boundary()
        self.assertFalse(self.fork.manifest.get("boundary_validated"))

    def test_rpc_wait_reports_before_polling_and_periodically_without_extending_deadline(self):
        self.fork.timeout = 60
        for becomes_ready in (True, False):
            with self.subTest(becomes_ready=becomes_ready):
                attempts = 0

                def request(*args):
                    nonlocal attempts
                    attempts += 1
                    self.assertTrue(output.called, "announce the wait before the first RPC request")
                    if attempts == 3:
                        self.assertGreaterEqual(output.call_count, 2, "report again during a long wait")
                        if becomes_ready:
                            return "0x2105"
                    raise devnet.Unavailable("provider-secret")

                with patch.object(self.fork, "url", return_value="https://rpc.invalid/provider-secret"), \
                        patch.object(devnet, "rpc", side_effect=request), \
                        patch.object(devnet.time, "monotonic", side_effect=[0, 1, 31, 61]), \
                        patch.object(devnet.time, "sleep"), patch("builtins.print") as output:
                    if becomes_ready:
                        self.fork.await_rpc("sequencer")
                    else:
                        with self.assertRaisesRegex(RuntimeError, "timed out: sequencer execution RPC"):
                            self.fork.await_rpc("sequencer")
                    self.assertEqual(attempts, 3)
                    self.assertIn("sequencer execution RPC", str(output.call_args_list))
                    self.assertNotIn("provider-secret", str(output.call_args_list))
                    self.assertTrue(all(call.kwargs.get("flush") for call in output.call_args_list))
                    self.assertTrue(all(call.kwargs.get("file") is sys.stderr for call in output.call_args_list))

    def test_compose_reports_service_actions_without_credentials_or_raw_output(self):
        with patch.object(self.fork, "compose_env", return_value={"SNAPSHOT_L1_RPC": "provider-secret"}), \
                patch.object(devnet, "run", return_value="raw-output-secret") as run, \
                patch("builtins.print") as output:
            self.fork.compose("up", "-d", "--no-build", "l1")
            run.assert_called_once()
            self.assertTrue(output.called, "announce container startup instead of silently capturing Compose")
            self.assertIn("l1", str(output.call_args_list))
            self.assertNotIn("secret", str(output.call_args_list))
            self.assertTrue(all(call.kwargs.get("flush") for call in output.call_args_list))
            self.assertTrue(all(call.kwargs.get("file") is sys.stderr for call in output.call_args_list))

    def test_progress_extends_stall_timeout_and_reports_without_secrets(self):
        clock = iter(range(0, 1000, 10))
        polls = iter([False] * 6 + [True])
        heads = iter(range(100))
        printed = []
        with patch.object(devnet.time, "monotonic", side_effect=lambda: next(clock)), \
                patch.object(devnet.time, "sleep"), \
                patch("builtins.print", side_effect=lambda *args, **_: printed.append(args)):
            self.assertTrue(devnet.wait("catch-up", lambda: next(polls), 15,
                                        progress=lambda: (next(heads), "head advanced"), report_interval=20))
            self.assertTrue(printed)
            with self.assertRaisesRegex(RuntimeError, "no progress for 15s.*head stuck"):
                devnet.wait("catch-up", lambda: False, 15, progress=lambda: (1, "head stuck"))

    @unittest.skipUnless(shutil.which("docker"), "requires Docker Compose, but does not start containers")
    def test_rendered_compose_has_private_nodes_immutable_images_and_only_l1_published(self):
        devnet.write_json(self.fork.directory / "keys.json", {"signer": "0x" + "1" * 64, "batcher": "0x" + "2" * 64})
        del self.fork.manifest["fork"]
        config = json.loads(self.fork.compose("--profile", "inspect", "config", "--format", "json"))
        self.assertTrue(config["networks"]["private"]["internal"])
        self.assertEqual(set(config["services"]["l1"]["networks"]), {"private", "upstream"})
        self.assertEqual([(port["host_ip"], port["published"]) for port in config["services"]["l1"]["ports"]],
                         [("127.0.0.1", "19545")])
        for name, service in config["services"].items():
            self.assertTrue(service["image"].startswith("sha256:"))
            if name != "l1":
                self.assertEqual(set(service["networks"]), {"private"})
                self.assertNotIn("ports", service)
            self.assertNotIn("restart", service)
            for volume in service.get("volumes", []):
                self.assertNotIn(".devnet", volume["source"])
        for name in ("sequencer", "validator"):
            command = config["services"][name]["command"]
            self.assertIn("--p2p.no-discovery", command)
            self.assertIn("--no-persist-peers", command)
            self.assertIn("--l1-slot-duration-override=12", command)
            self.assertNotIn("--rollup.sequencer", " ".join(command))
            self.assertIn("sequencer", command)
            self.assertIn("--sequencer.stopped", command)
        # The batcher's default throttling calls miner_setMaxDASize on the sequencer's HTTP RPC.
        sequencer_command = config["services"]["sequencer"]["command"]
        http_apis = next(arg.split("=", 1)[1].split(",") for arg in sequencer_command if arg.startswith("--http.api="))
        self.assertIn("miner", http_apis)
        self.assertEqual(config["services"]["batcher"]["stop_signal"], "SIGTERM")


class QualificationTests(unittest.TestCase):
    def test_denim_activation_uses_genesis_slot_parity_not_absolute_even_seconds(self):
        config = {"genesis": {"l2_time": 101}, "block_time": 2}
        for scheduled, first_block in ((100, 101), (101, 101), (1000, 1001), (1001, 1001), (1002, 1003)):
            with self.subTest(scheduled=scheduled):
                self.assertEqual(verification.denim_activation_time(config, scheduled), first_block)

    def test_denim_window_waits_while_heads_advance_but_rejects_stalls_and_late_arrival(self):
        for times, error in (((900, 910, 920, 930, 940, 994), None),
                             ((900,) * 8, "no progress"), ((998,), "too late")):
            with self.subTest(times=times):
                clock = iter(range(0, 1000, 10))
                fork = devnet.SnapshotFork("/unused", timeout=15)
                headers = [{"number": hex(t), "timestamp": hex(t)} for t in times]
                with patch.object(fork, "url", return_value="http://sequencer"), \
                        patch.object(verification, "rpc", side_effect=headers) as rpc, \
                        patch.object(devnet.time, "monotonic", side_effect=lambda: next(clock)), \
                        patch.object(devnet.time, "sleep"), patch("builtins.print"):
                    if error:
                        with self.assertRaisesRegex(RuntimeError, error):
                            verification.wait_denim_window(fork, 1000)
                    else:
                        verification.wait_denim_window(fork, 1000)
                        self.assertEqual(rpc.call_count, 6)

    def window(self):
        before = {"timestamp": "0x3e6", "hash": "before", "gasLimit": hex(40_000_030),
                  "extraData": "0x010000003d00000006000000000000000b"}
        blocks = []
        parent = "before"
        for index, milliseconds in enumerate((0, 200, 400, 600, 800, 0, 200)):
            header = {"timestamp": hex(1000 + (index >= 5)), "timestampMs": hex(1_000_000 + 200 * index),
                      "parentHash": parent, "hash": str(index), "gasLimit": hex(4_000_003),
                      "extraData": "0x010000026200000006000000000000000b",
                      "transactions": [{"type": "0x7e"}, {"type": "0x7e", "to": verification.BASE_TIME,
                          "input": "0x86bdf394" + f"{milliseconds:064x}"}, {"type": "0x2"}]}
            blocks.append(header)
            parent = str(index)
        return before, blocks

    def test_denim_checks_activation_siblings_rollover_and_single_scaling(self):
        before, blocks = self.window()
        verification.check_denim_window(before, blocks, 1000)
        for index, field, value in ((0, "gasLimit", hex(40_000_030)), (1, "gasLimit", hex(400_000)),
                                     (5, "timestampMs", hex(1_000_800)),
                                     (6, "extraData", "0x01000017d400000006000000000000000b")):
            changed = copy.deepcopy(blocks)
            changed[index][field] = value
            with self.assertRaises(RuntimeError):
                verification.check_denim_window(before, changed, 1000)
        blocks[2]["transactions"][1], blocks[2]["transactions"][2] = blocks[2]["transactions"][2], blocks[2]["transactions"][1]
        with self.assertRaisesRegex(RuntimeError, r"tx\[1\]"):
            verification.check_denim_window(before, blocks, 1000)

    def test_matching_height_is_not_parity(self):
        with self.assertRaisesRegex(RuntimeError, "state root mismatch"):
            verification.check_parity({"hash": "a", "stateRoot": "a"}, {"hash": "a", "stateRoot": "b"})

    @unittest.skipUnless(os.environ.get("BASE_SNAPSHOT_FORK_DIR"), "opt-in real snapshot fork qualification")
    def test_live_snapshot_fork(self):
        command = [sys.executable, str(Path(verification.__file__)), "--dir", os.environ["BASE_SNAPSHOT_FORK_DIR"], "--restart"]
        if os.environ.get("BASE_SNAPSHOT_TEST_DENIM") == "1":
            command += ["--denim"]
        subprocess.run(command, check=True)


if __name__ == "__main__":
    unittest.main()
