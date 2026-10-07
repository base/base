#!/usr/bin/env python3
"""Offline launcher tests; optional live qualification requires BASE_SNAPSHOT_FORK_DIR."""

import contextlib
import copy
import fcntl
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import DEFAULT, Mock, patch

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


def completed(**fields):
    """A manifest whose init finished runtime bootstrap and stopped every service."""
    return {**manifest(), "phase": "stopped", "boundary_validated": True, "bootstrapped": True,
            "denim_timestamp": 1000, **fields}


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


class FakeRuntime:
    """Local L1, L2 RPCs and Compose for prepare_runtime. L1 state survives attempts like Anvil's saved
    dump, and a write takes effect only when mined, as with interval mining paused."""

    MOCK = "0x" + "4" * 40
    HISTORY = [100] * 12 + [800]
    SYSTEM_READS = {"setBatcherHash(bytes32)": "batcherHash()(bytes32)",
                    "setUnsafeBlockSigner(address)": "unsafeBlockSigner()(address)"}

    def __init__(self, test, directory, roles, legacy=False):
        self.test = test
        self.fork = devnet.SnapshotFork(directory, timeout=1)
        self.fork.directory.mkdir()
        self.fork.manifest = {**manifest(), "schedule": list(self.HISTORY), "minimum_protocol_version": 42,
                              "system_config": "0x" + "1" * 40, "initial": [snapshot()],
                              "datadirs": {role: f"/unused-{role}" for role in roles}}
        if not legacy:
            self.fork.manifest.update(fast_denim=True, local_protocol_versions=self.MOCK, upgrade_code="0x6000")
        self.fork.save()
        self.head, self.timestamp, self.code = 100, 1234, {}
        self.contracts = {devnet.PROTOCOL_VERSIONS: {"schedule": list(self.HISTORY), "minimum": 42},
                          self.MOCK: {"schedule": [], "minimum": 0}}
        self.system = {"batcherHash()(bytes32)": "0x0", "unsafeBlockSigner()(address)": "0x0"}
        self.pending, self.receipts, self.sent, self.rpcs, self.commands = [], {}, [], [], []
        self.failure, self.saves_dump, self.mined, self.dump_ns = None, True, 0, 10**18

    def fail_once(self, predicate, error):
        self.failure = (predicate, error)

    def check(self, *event):
        if self.failure and self.failure[0](*event):
            error, self.failure = self.failure[1], None
            raise error

    def run(self, *args, **_):
        self.test.assertEqual(args[:2], ("cast", "calldata"))
        return "0x" + json.dumps(args[2:]).encode().hex()

    def rpc(self, url, method, *params, upstream=False):
        self.test.assertFalse(upstream)
        self.rpcs.append((url, method, *params))
        if method in ("eth_estimateGas", "eth_sendTransaction"):
            signature, *args = json.loads(bytes.fromhex(params[0]["data"][2:]))
            self.check(method, signature, args)
            if method == "eth_estimateGas":
                return "0x5208"
            transaction_hash = f"0xtx{len(self.sent)}"
            self.sent.append(signature)
            self.pending.append((transaction_hash, params[0]["to"], signature, args))
            return transaction_hash
        if url != "l1":
            if method == "admin_sequencerActive":
                return False
            self.test.assertEqual(method, "eth_getBlockByNumber")
            return {"hash": "0x123", "timestamp": hex(1234)} if params[0] in ("latest", hex(123)) else None
        if method == "eth_getBlockByNumber":
            if params[0] == "latest":
                return {"number": hex(self.head), "timestamp": hex(self.timestamp)}
            height = int(params[0], 16)
            return {"number": params[0], "hash": f"0xl1{height}",
                    "parentHash": "0xf" if height == 101 else f"0xl1{height - 1}"} if height <= self.head else None
        if method == "anvil_setCode":
            self.code[params[0]] = params[1]
        results = {"eth_getCode": lambda: self.code.get(params[0], "0x"), "eth_blockNumber": lambda: hex(self.head),
                   "eth_getTransactionCount": lambda: hex(len(self.sent)),
                   "eth_getTransactionReceipt": lambda: self.receipts.get(params[0])}
        if method in results:
            return results[method]()
        self.test.assertIn(method, ("anvil_setCode", "anvil_setBalance", "anvil_impersonateAccount",
                                    "anvil_stopImpersonatingAccount"))
        return None

    def call(self, url, address, signature, *args, **_):
        self.test.assertEqual(url, "l1")
        reads = {"getSchedule()(uint64[])": lambda: list(map(str, self.contracts[address]["schedule"])),
                 "minimumProtocolVersion()(uint256)": lambda: str(self.contracts[address]["minimum"]),
                 "MIN_NOTICE()(uint64)": lambda: "3600"}
        return reads[signature]() if signature in reads else self.system.get(signature, "0x" + "a" * 40)

    def mine(self):
        self.mined += 1
        self.head += 1
        self.timestamp = int(devnet.time.time())
        for transaction_hash, target, signature, args in self.pending:
            if signature == "setSchedule(uint64[])":
                self.contracts[target]["schedule"] = json.loads(args[0])
            elif signature == "registerUpgrade(uint64,uint256)":
                self.contracts[target]["schedule"].append(int(args[0]))
            elif signature == "setMinimumProtocolVersion(uint256)":
                self.contracts[target]["minimum"] = int(args[0])
            else:
                self.system[self.SYSTEM_READS[signature]] = args[0]
            self.receipts[transaction_hash] = {"status": "0x1", "blockNumber": hex(self.head)}
        self.pending.clear()

    def compose(self, *args):
        self.commands.append(args)
        if args == ("stop", "l1") and self.saves_dump:
            dump = self.fork.directory / "l1/anvil.json"
            dump.parent.mkdir(exist_ok=True)
            dump.write_text("{}")
            self.dump_ns += 1
            os.utime(dump, ns=(self.dump_ns, self.dump_ns))

    def retry(self):
        """Reloads the durable manifest, as a new launcher process would, and resumes init."""
        self.fork.manifest = devnet.SnapshotFork(self.fork.directory).manifest
        self.fork.prepare_runtime()

    @contextlib.contextmanager
    def patched(self):
        fork = self.fork
        with patch.object(devnet, "validate_paths"), patch.multiple(fork, endpoint=DEFAULT, assert_local_l1=DEFAULT), \
                patch.object(fork, "await_rpc", side_effect=lambda role: self.check("await_rpc", role)), \
                patch.object(fork, "wait_upgrades", side_effect=lambda: self.check("wait_upgrades")), \
                patch.object(fork, "running", return_value=False), \
                patch.object(fork, "compose", side_effect=self.compose), \
                patch.object(fork, "consensus_ready", return_value=True), \
                patch.object(fork, "url", side_effect=lambda role: role), \
                patch.object(fork, "mine", side_effect=self.mine), \
                patch.object(fork, "sync_status", side_effect=lambda role: sync_status(101)), \
                patch.object(devnet, "rpc", side_effect=self.rpc), patch.object(devnet, "call", side_effect=self.call), \
                patch.object(devnet, "run", side_effect=self.run), \
                patch.object(devnet.time, "time", return_value=1234), patch.object(devnet.time, "sleep"), \
                patch("builtins.print"):
            yield


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

    @contextlib.contextmanager
    def starting(self):
        """Ready local services; each scenario supplies RPC responses and lifecycle failures."""
        with patch.object(devnet, "validate_paths"), \
                patch.multiple(self.fork, endpoint=DEFAULT, await_rpc=DEFAULT, assert_local_l1=DEFAULT,
                               validate_restored_contracts=DEFAULT, wait_upgrades=DEFAULT, peers=DEFAULT), \
                patch.object(self.fork, "running", return_value=False), \
                patch.object(self.fork, "compose"), \
                patch.object(self.fork, "inspect", return_value=[]), \
                patch.object(self.fork, "running_services", return_value=devnet.FORK_SERVICES), \
                patch.object(self.fork, "consensus_ready", return_value=True), \
                patch.object(self.fork, "url", side_effect=lambda role: role), \
                patch.object(devnet.time, "time", return_value=1234):
            yield

    def test_up_and_down_dispatch_to_nondestructive_lifecycle(self):
        self.fork.save()
        for command, method in (("up", "start"), ("down", "stop")):
            with self.subTest(command=command), \
                    patch.object(sys, "argv", ["snapshot_devnet.py", command, "--dir", str(self.fork.directory)]), \
                    patch.object(devnet.SnapshotFork, method) as lifecycle:
                devnet.main()
                lifecycle.assert_called_once_with()

    def test_clear_stops_saved_forks_preserves_data_and_prevents_bare_up(self):
        self.fork.save()
        pending = self.root / "pending"
        pending.mkdir()
        devnet.write_json(pending / "manifest.json", manifest())
        devnet.setup_path().parent.mkdir(parents=True)
        devnet.write_json(devnet.setup_path(), {"directory": str(self.fork.directory), "pending_directory": str(pending)})
        (devnet.setup_path().parent / "l1.env").write_text("keep credentials")
        before = {path: path.read_bytes() for path in (self.fork.directory / "manifest.json", pending / "manifest.json",
                                                       devnet.setup_path().parent / "l1.env")}

        def stop(fork):
            self.assertTrue(devnet.setup_path().exists(), "forget only after shutdown succeeds")
            with open(fork.directory / ".lock", "a") as lock:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)

        with patch.object(sys, "argv", ["launcher", "clear"]), \
                patch.object(devnet.SnapshotFork, "stop", autospec=True, side_effect=stop) as stopped, \
                patch.object(devnet, "run") as run, patch("builtins.print"):
            devnet.main()
            devnet.main()  # Already cleared: no Docker calls or other side effects.
            self.assertEqual([call.args[0].directory for call in stopped.call_args_list], [self.fork.directory, pending])
            self.assertFalse(devnet.setup_path().exists())
            self.assertEqual({path: path.read_bytes() for path in before}, before)
            with patch.object(sys, "argv", ["launcher", "up"]):
                with self.assertRaisesRegex(RuntimeError, "snapshot setup has not completed"):
                    devnet.main()
            run.assert_not_called()

    def test_clear_keeps_selection_when_shutdown_fails_or_another_command_holds_lock(self):
        self.fork.save()
        devnet.setup_path().parent.mkdir(parents=True)
        devnet.write_json(devnet.setup_path(), {"directory": str(self.fork.directory)})
        before = devnet.setup_path().read_bytes()
        with patch.object(sys, "argv", ["launcher", "clear"]), \
                patch.object(devnet.SnapshotFork, "stop", side_effect=RuntimeError("shutdown failed")) as stopped:
            with open(self.fork.directory / ".lock", "a") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                with self.assertRaises(BlockingIOError):
                    devnet.main()
            stopped.assert_not_called()
            with self.assertRaisesRegex(RuntimeError, "shutdown failed"):
                devnet.main()
        self.assertEqual(devnet.setup_path().read_bytes(), before)

    def test_up_returns_while_far_behind_without_revalidating_or_rescheduling(self):
        self.fork.manifest.update(phase="stopped", boundary_validated=True, bootstrapped=True,
                                  denim_timestamp=1000, last_stop={"sequencer": sync_status(102, safe=140)})
        (self.fork.directory / "l1").mkdir()
        (self.fork.directory / "l1/anvil.json").write_text("{}")

        def node(url, method, *args):
            if method == "eth_getBlockByNumber":
                return {"hash": "0xhead", "timestamp": "0x64"}
            return method != "admin_sequencerActive"

        with self.starting(), patch.object(self.fork, "mine"), \
                patch.object(self.fork, "inspect", side_effect=AssertionError("restart must not inspect snapshots")), \
                patch.object(self.fork, "validate_restored_contracts", side_effect=AssertionError("init validates contracts")), \
                patch.object(self.fork, "sync_status", side_effect=AssertionError("restart must not wait for heads")), \
                patch.object(self.fork, "schedule_denim", side_effect=AssertionError("restart must retain the schedule")), \
                patch.object(devnet, "rpc", side_effect=node) as transport, patch("builtins.print"):
            self.fork.start()
            self.assertEqual(self.fork.manifest["phase"], "running")
            self.assertIn(("sequencer-cl", "admin_startSequencer", "0xhead"),
                          [call.args for call in transport.call_args_list])
            self.assertIn(("up", "-d", "--no-build", "batcher"),
                          [call.args for call in self.fork.compose.call_args_list])
        self.assertEqual(self.fork.manifest["denim_timestamp"], 1000)

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
        self.fork.manifest = completed()
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
                self.assertEqual(fork.timeout, 7200)
                self.assertEqual(fork.endpoint("execution"), "https://rpc.invalid/secret-key")
        with patch.object(sys, "argv", ["verify"]), patch.object(verification, "verify") as verify:
            verification.main()
            self.assertEqual(verify.call_args.args[0].directory, self.fork.directory)
            self.assertEqual(verify.call_args.args[0].timeout, 7200)
            self.assertEqual(len(verify.call_args.args), 1)
        devnet.write_json(self.fork.directory / "keys.json", {"signer": "0x1", "batcher": "0x2"})
        fork = devnet.SnapshotFork(self.fork.directory)
        self.assertEqual(fork.timeout, 7200)
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

    def test_setup_and_download_reject_nonpositive_concurrency_before_prompting(self):
        for task in ("setup", "download"):
            for concurrency in ("0", "-1"):
                with self.subTest(task=task, concurrency=concurrency), \
                        patch.object(sys, "argv", ["launcher", task, "--dir", str(self.root / "new"),
                                                   "--download-concurrency", concurrency]), \
                        patch("builtins.input") as prompt, patch.object(devnet, "setup_command") as command:
                    with self.assertRaisesRegex(RuntimeError, "download concurrency must be positive"):
                        devnet.main()
                    prompt.assert_not_called()
                    command.assert_not_called()
                    self.assertFalse((self.root / "new").exists())

    def test_download_resumes_pinned_snapshot_without_setup_or_copy_and_completed_download_is_noop(self):
        directory = self.root / "snapshot with spaces"
        devnet.setup_path().parent.mkdir(parents=True)
        devnet.write_json(devnet.setup_path(), {"directory": "/another/fork", "pending_directory": "/pending/fork"})
        selection = devnet.setup_path().read_bytes()
        image = "sha256:" + "b" * 64
        with patch.object(sys, "argv", ["launcher", "download", "--dir", str(directory)]), \
                patch.object(devnet, "run", return_value=image) as inspect_image, \
                patch.object(devnet, "request_json", side_effect=[[
                    {"chainId": 8453, "block": 123, "metadataUrl": "https://snapshot.invalid/123/manifest.json"}],
                    {"chain_id": 8453, "block": 123}]) as metadata, \
                patch.object(devnet, "setup_command", side_effect=[KeyboardInterrupt(), None]) as command, \
                patch.object(devnet.SnapshotFork, "initialize", side_effect=AssertionError("download only")), \
                patch.object(devnet.getpass, "getpass", side_effect=AssertionError("no credentials needed")), \
                patch("builtins.print"):
            with self.assertRaises(KeyboardInterrupt):
                devnet.main()
            journal = directory / "snapshot-download.json"
            self.assertFalse(json.loads(journal.read_text())["complete"])
            pinned = (directory / "download-manifest.json").read_bytes()
            with patch.object(sys, "argv", ["launcher", "download", "--dir", str(directory),
                                           "--download-concurrency", "32"]):
                devnet.main()
            self.assertTrue(json.loads(journal.read_text())["complete"])
            devnet.main()
            self.assertEqual(command.call_count, 2)
            for concurrency, invocation in zip(("16", "32"), command.call_args_list):
                args = invocation.args
                self.assertEqual(args[:3], ("docker", "run", "--rm"))
                self.assertEqual(args[args.index("--entrypoint") + 1:args.index("--chain")],
                                 ("/app/base", image, "snapshot", "download"))
                self.assertEqual(args[args.index("--datadir") + 1], "/work")
                self.assertEqual(args[args.index("-v") + 1], f"{directory}:/work")
                self.assertEqual(args[args.index("--download-concurrency") + 1], concurrency)
                self.assertNotIn("--force", args)
                for flag in ("--with-txs-distance", "--with-receipts-distance", "--with-state-history-distance"):
                    self.assertEqual(args[args.index(flag) + 1], "1339200")
            self.assertEqual((directory / "download-manifest.json").read_bytes(), pinned)
            self.assertEqual(metadata.call_count, 2, "retry must not discover a newer snapshot")
            inspect_image.assert_called_once()
            self.assertEqual(devnet.setup_path().read_bytes(), selection)
            (directory / "download-manifest.json").unlink()
            with self.assertRaisesRegex(RuntimeError, "pinned download manifest is missing"):
                devnet.main()
            self.assertEqual(command.call_count, 2)

    def test_download_refuses_existing_datadir_and_overlapping_download(self):
        directory = self.datadir("existing")
        with patch.object(sys, "argv", ["launcher", "download", "--dir", str(directory)]), \
                patch.object(devnet, "run") as run, patch.object(devnet, "setup_command") as command:
            with self.assertRaisesRegex(RuntimeError, "existing data preserved"):
                devnet.main()
            with open(directory / ".download.lock", "a") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                with self.assertRaises(BlockingIOError):
                    devnet.main()
            run.assert_not_called()
            command.assert_not_called()
            self.assertEqual((directory / "db/mdbx.dat").read_bytes(), b"untouched")

    def test_download_missing_image_prints_build_command_without_selecting_snapshot(self):
        with patch.object(sys, "argv", ["launcher", "download", "--dir", str(self.root / "download")]), \
                patch.object(devnet, "run", side_effect=RuntimeError("image missing")), \
                patch.object(devnet, "request_json") as metadata, patch.object(devnet, "setup_command") as command:
            with self.assertRaisesRegex(RuntimeError, "docker buildx bake"):
                devnet.main()
            metadata.assert_not_called()
            command.assert_not_called()
            self.assertFalse(devnet.setup_path().exists())

    def test_setup_builds_missing_defaults_pulls_remote_and_preserves_custom_images(self):
        image_id = "sha256:" + "f" * 64
        for source in ("cached", "missing", "remote", "custom", "pinned"):
            with self.subTest(source=source):
                work = self.root / source
                work.mkdir()
                fork = devnet.SnapshotFork(work / "fork")
                args = Mock(anvil_image="base-anvil:snapshot-24ec5e47", batcher_image="op-batcher:local")
                if source in ("remote", "custom", "pinned"):
                    args.anvil_image = {"remote": "ghcr.io/example/anvil:custom", "custom": "anvil:custom",
                                        "pinned": image_id}[source]
                commands = []

                def command(*command, lock_fd):
                    self.assertEqual(lock_fd, lock.fileno())
                    if command[0] == "cargo":
                        raise RuntimeError("inspector interrupted")
                    commands.append(command)

                def docker(*command):
                    if len(command) == 4 and source in ("missing", "custom"):
                        raise RuntimeError("image missing")
                    return image_id

                with tempfile.TemporaryFile() as lock, \
                        patch.object(devnet.shutil, "which", return_value="/usr/bin/tool"), \
                        patch.object(devnet, "run", side_effect=docker), \
                        patch.object(devnet, "setup_command", side_effect=command), patch("builtins.print"):
                    with self.assertRaisesRegex(RuntimeError, "image missing" if source == "custom" else "inspector interrupted"):
                        devnet.prepare_snapshot(args, fork, work, lock)
                self.assertEqual(commands[0][:3], ("docker", "buildx", "bake"))
                if source == "missing":
                    self.assertEqual(commands[1], ("just", "devnet", "snapshot", "build-anvil"))
                    self.assertEqual(commands[2][:4], ("docker", "build", "-f", "etc/docker/Dockerfile.op-batcher"))
                elif source == "remote":
                    self.assertEqual(commands[1], ("docker", "pull", "ghcr.io/example/anvil:custom"))
                else:
                    self.assertEqual(len(commands), 1)
                images = json.loads((work / "setup.json").read_text())["images"]
                self.assertEqual(images["base"], image_id)
                self.assertEqual(images["anvil"], "anvil:custom" if source == "custom" else image_id)

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

                def initialize(fork, config):
                    fork.manifest = manifest()
                    fork.manifest["upstreams"] = {"execution": "SNAPSHOT_UPSTREAM_EXECUTION",
                                                  "beacon": "SNAPSHOT_UPSTREAM_BEACON"}
                    self.assertTrue(config["fast_denim"])
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

                def initialize(fork, config):
                    initialized.append(config)
                    fork.manifest = {**manifest(), "phase": "inspecting", "setup_input": config}
                    fork.save()
                    # If init has touched either datadir, no retry may copy builder onto validator.
                    (work / "validator/db/mdbx.dat").write_bytes(b"opened-by-inspection")
                    fail("initialize")
                    fork.manifest = completed(setup_input=config)
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

    def test_setup_resumes_runtime_init_without_preparing_or_rediscovering(self):
        self.fork.manifest.update(phase="initializing", setup_input={"sequencer_datadir": "/unused-sequencer"})
        self.fork.save()
        devnet.setup_path().parent.mkdir(parents=True)
        devnet.write_json(devnet.setup_path(), {"directory": "/previous", "pending_directory": str(self.fork.directory)})
        attempts = []

        def resume(fork):
            attempts.append(fork.manifest["phase"])
            if len(attempts) == 1:
                raise RuntimeError("Denim receipt timed out")
            fork.manifest = completed()
            fork.save()

        with patch.object(sys, "argv", ["launcher", "setup"]), \
                patch.dict(os.environ, {"TEST_EXECUTION_URL": "https://rpc.invalid/key",
                                        "TEST_BEACON_URL": "https://beacon.invalid/key"}), \
                patch.object(devnet, "rpc", return_value="0x1"), \
                patch.object(devnet, "request_json", side_effect=lambda url: self.assertNotEqual(
                    url, devnet.SNAPSHOT_INDEX) or {"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                patch.object(devnet, "setup_command") as command, \
                patch.object(devnet.SnapshotFork, "initialize", side_effect=AssertionError("F is already known")), \
                patch.object(devnet.SnapshotFork, "prepare_runtime", autospec=True, side_effect=resume), \
                patch("builtins.input", side_effect=AssertionError("pending fork is remembered")), \
                patch("builtins.print"):
            with self.assertRaisesRegex(RuntimeError, "Denim receipt timed out"):
                devnet.main()
            self.assertEqual(json.loads(devnet.setup_path().read_text())["pending_directory"], str(self.fork.directory))
            devnet.main()
            devnet.main()
            command.assert_not_called()
        self.assertEqual(attempts, ["initializing", "initializing"], "completed init must not resume again")
        self.assertEqual(json.loads(devnet.setup_path().read_text()), {"directory": str(self.fork.directory)})

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

        def initialize(fork, saved):
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

    def test_direct_init_resumes_without_downloading_copying_or_changing_pruning(self):
        for paired in (False, True):
            with self.subTest(paired=paired):
                source = self.datadir(f"data {paired}")
                validator = self.datadir(f"validator {paired}") if paired else None
                (source / "reth.toml").write_text("[prune.segments]\nreceipts = { distance = 1339200 }\n")
                original = (source / "reth.toml").read_bytes()
                work = source.with_name(source.name + "-devnet")
                roles = {"sequencer": str(source), **({"validator": str(validator)} if paired else {})}
                image = "sha256:" + "e" * 64
                initialized = []

                def initialize(fork, config):
                    initialized.append(config)
                    if len(initialized) == 1:
                        raise RuntimeError("waiting for finalized L1")
                    fork.manifest = completed(datadirs=roles, fast_denim=True, setup_input=config)
                    fork.save()

                arguments = ["launcher", "init", "--dir", str(source)]
                if validator:
                    arguments += ["--validator-dir", str(validator)]
                with patch.dict(os.environ, {"SNAPSHOT_UPSTREAM_EXECUTION": "https://rpc.invalid/key",
                                              "SNAPSHOT_UPSTREAM_BEACON": "https://beacon.invalid/key"}), \
                        patch.object(devnet, "rpc", return_value="0x1"), \
                        patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                        patch.object(devnet, "run", return_value=image), \
                        patch.object(devnet.shutil, "which", side_effect=lambda tool: None if tool == "rsync" else tool), \
                        patch.object(devnet, "setup_command") as commands, \
                        patch.object(devnet.SnapshotFork, "initialize", autospec=True, side_effect=initialize), \
                        patch("builtins.input", side_effect=AssertionError("datadir is already supplied")), \
                        patch.object(devnet.getpass, "getpass", side_effect=AssertionError("endpoints already supplied")), \
                        patch("builtins.print"):
                    with patch.object(sys, "argv", arguments):
                        with self.assertRaisesRegex(RuntimeError, "finalized L1"):
                            devnet.main()
                    built = commands.call_count
                    with patch.object(sys, "argv", arguments):
                        devnet.main()
                        devnet.main()
                    self.assertEqual(commands.call_count, built)
                    self.assertEqual([c.args[0] for c in commands.call_args_list], ["docker", "cargo"])
                    self.assertEqual(commands.call_args_list[0].args[1:3], ("buildx", "bake"))
                self.assertEqual(initialized[0], initialized[1])
                self.assertTrue(initialized[0]["fast_denim"])
                self.assertEqual({k.removesuffix("_datadir"): v for k, v in initialized[0].items()
                                  if k.endswith("_datadir")}, roles)
                self.assertEqual(devnet.configured_directory(), work / "fork")
                self.assertEqual((source / "reth.toml").read_bytes(), original)
                self.assertEqual((source / "db/mdbx.dat").read_bytes(), b"untouched")
                self.assertFalse((work / "download-manifest.json").exists())
                self.assertFalse((work / "builder").exists())
                self.assertFalse((work / "validator").exists())

    def test_init_retry_preserves_identity_and_keys_and_completed_init_is_a_noop(self):
        self.check_init_retry(paired=True)

    def test_single_node_init_defaults_to_mock_and_pins_it_across_interruption(self):
        self.check_init_retry(paired=False)

    def test_legacy_init_resume_keeps_original_contract_without_installing_mock(self):
        self.check_init_retry(paired=True, legacy=True)

    def check_init_retry(self, paired, legacy=False):
        builder = self.datadir("builder")
        image = "sha256:" + "d" * 64
        config = {"sequencer_datadir": str(builder),
                  **{role + "_image": image for role in ("base", "anvil", "batcher")}}
        if paired:
            config["validator_datadir"] = str(self.datadir("validator"))
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

        def run(*args):
            if args[0] == "docker":
                return image
            if args[0] == "forge":
                return "0x6000"
            return "0x" + args[-1][-40:]

        inspections = [{**copy.deepcopy(initial), "fork": discovered}]
        if paired:
            inspections.append(copy.deepcopy(initial))
        with patch.object(sys, "argv", ["launcher", "init", "--dir", str(self.fork.directory),
                                       "--config", str(config_path)]), \
                patch.object(devnet, "run", side_effect=run), \
                patch.dict(os.environ, {"SNAPSHOT_UPSTREAM_EXECUTION": "https://rpc.invalid/key",
                                        "SNAPSHOT_UPSTREAM_BEACON": "https://rpc.invalid/key"}), \
                patch.object(devnet, "rpc", side_effect=transport), \
                patch.object(devnet, "call", side_effect=contract), \
                patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                patch.object(devnet.SnapshotFork, "inspect", side_effect=[KeyboardInterrupt(), inspections]) as inspect, \
                patch.object(devnet.SnapshotFork, "prepare_runtime", autospec=True) as runtime, \
                patch("builtins.print"):
            with self.assertRaises(KeyboardInterrupt):
                devnet.main()
            before = devnet.SnapshotFork(self.fork.directory).manifest
            self.assertTrue(before["fast_denim"], "new init must use the mock without a flag or config field")
            if legacy:
                # An interrupted initialization from the old launcher has none of these fields.
                for field in ("fast_denim", "local_protocol_versions", "upgrade_code"):
                    del before[field]
                devnet.write_json(self.fork.directory / "manifest.json", before)
            keys = (self.fork.directory / "keys.json").read_bytes()
            self.assertEqual(before["phase"], "inspecting")
            runtime.assert_not_called()
            devnet.main()
            # Metadata is durable before runtime bootstrap, which --config init also performs.
            runtime.assert_called_once()
            after = devnet.SnapshotFork(self.fork.directory).manifest
            self.assertEqual(after["phase"], "prepared")
            self.assertEqual(after["project"], before["project"])
            self.assertEqual(after["accounts"], before["accounts"])
            self.assertEqual((self.fork.directory / "keys.json").read_bytes(), keys)
            if legacy:
                self.assertNotIn("fast_denim", after)
                self.assertNotIn("local_protocol_versions", after)
                self.assertNotIn("upgrade_code", after)
            else:
                self.assertEqual(after["local_protocol_versions"], before["local_protocol_versions"])
                self.assertNotEqual(after["local_protocol_versions"], after["protocol_versions"])
                self.assertEqual(after["upgrade_code"], "0x6000")
            if not paired:
                self.assertEqual(set(after["datadirs"]), {"sequencer"})
                self.assertFalse((self.fork.directory / "validator").exists())
            devnet.main()
            self.assertEqual(inspect.call_count, 2, "runtime retry must not rediscover F")
            self.assertEqual(runtime.call_count, 2)
            self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest, after)
            devnet.write_json(config_path, {**config, "epoch_slots": 99})
            with self.assertRaisesRegex(RuntimeError, "config changed"):
                devnet.main()
            self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest, after)
            devnet.write_json(config_path, {**config, "fast_denim": legacy})
            with self.assertRaisesRegex(RuntimeError, "activation mode is pinned"):
                devnet.main()
            self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest, after)

    def test_up_reuses_running_services_and_recovers_partial_start(self):
        self.fork.manifest = completed(phase="running")
        unchanged = AssertionError("an active fork must not be revalidated or rescheduled")
        with patch.object(self.fork, "running", return_value=True), \
                patch.object(self.fork, "running_services", return_value=set(devnet.FORK_SERVICES)) as services, \
                patch.object(self.fork, "assert_local_l1", side_effect=unchanged), \
                patch.object(self.fork, "validate_restored_contracts", side_effect=unchanged), \
                patch.object(self.fork, "consensus_ready", return_value=True), \
                patch.object(self.fork, "url", return_value="http://sequencer-cl"), \
                patch.object(devnet, "rpc", return_value=True) as active, \
                patch.object(self.fork, "schedule_denim", side_effect=unchanged), \
                patch.object(self.fork, "stop") as stop, \
                patch.object(self.fork, "inspect") as inspect, \
                patch.object(self.fork, "compose") as compose, \
                patch.object(devnet, "validate_paths", side_effect=RuntimeError("recovery reached")) as paths, \
                patch("builtins.print"):
            self.fork.start()
            stop.assert_not_called()
            paths.assert_not_called()
            inspect.assert_not_called()
            compose.assert_not_called()
            self.assertEqual(self.fork.manifest["denim_timestamp"], 1000)
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

    def test_init_bootstraps_runtime_with_mining_paused_and_leaves_services_stopped(self):
        seeded = ["setSchedule(uint64[])", "setMinimumProtocolVersion(uint256)"]
        configured = ["setBatcherHash(bytes32)", "setUnsafeBlockSigner(address)"]
        # Legacy forks keep the restored contract's notice; fast forks schedule about 60s after wall clock.
        for roles, legacy, denim, sent, mined in (
            (("sequencer",), False, 1294, seeded + configured + ["setSchedule(uint64[])"], 5),
            (devnet.ROLES, False, 1294, seeded + configured + ["setSchedule(uint64[])"], 5),
            (devnet.ROLES, True, 4858, configured + ["registerUpgrade(uint64,uint256)"], 4),
        ):
            with self.subTest(roles=roles, legacy=legacy):
                runtime = FakeRuntime(self, self.root / f"init-{len(roles)}-{legacy}", roles, legacy)
                with runtime.patched():
                    runtime.fork.prepare_runtime()
                stored = devnet.SnapshotFork(runtime.fork.directory)
                self.assertTrue(stored.initialized())
                self.assertEqual(stored.manifest["phase"], "stopped")
                self.assertEqual(stored.manifest["denim_timestamp"], denim)
                self.assertEqual(runtime.contracts[stored.upgrade_contract]["schedule"], runtime.HISTORY + [denim])
                self.assertEqual(runtime.sent, sent)
                # One wall-slot block per write; legacy forks also mine F's successor for the boundary.
                self.assertEqual(runtime.mined, mined)
                self.assertEqual(stored.manifest["boundary"]["l1_successor"], {"number": "0x65", "hash": "0xl1101"})
                self.assertEqual(runtime.commands, [("up", "-d", "--no-build", "l1"), ("up", "-d", "--no-build", *roles),
                                                    ("stop", *roles), ("stop", "l1")])
                self.assertEqual("validator" in str(runtime.rpcs), "validator" in roles)
                if not legacy:
                    self.assertEqual(runtime.contracts[devnet.PROTOCOL_VERSIONS]["schedule"], runtime.HISTORY)

    def test_init_retry_after_bootstrap_or_scheduling_never_repeats_committed_writes(self):
        def denim(*event):
            # The 14-entry setSchedule is the Denim write; the 13-entry one seeds history.
            return event[:2] == ("eth_estimateGas", "setSchedule(uint64[])") and len(json.loads(event[2][0])) == 14

        cases = (
            ("during bootstrap", lambda *event: event[:2] == ("eth_estimateGas", "setUnsafeBlockSigner(address)"),
             RuntimeError("estimate failed")),
            ("after bootstrap", denim, KeyboardInterrupt()),
            ("after scheduling", lambda *event: event == ("wait_upgrades",), RuntimeError("nodes did not observe")),
        )
        for name, predicate, error in cases:
            for roles in (("sequencer",), devnet.ROLES):
                with self.subTest(name, roles=roles):
                    runtime = FakeRuntime(self, self.root / f"{name}-{len(roles)}", roles)
                    runtime.fail_once(predicate, error)
                    fork, dump = runtime.fork, runtime.fork.directory / "l1/anvil.json"
                    with runtime.patched():
                        with self.assertRaises(type(error)):
                            fork.prepare_runtime()
                        self.assertEqual(runtime.commands[-2:], [("stop", *roles), ("stop", "l1")])
                        self.assertEqual(fork.manifest["phase"], "initializing")
                        self.assertEqual(fork.manifest.get("bootstrapped", False), name != "during bootstrap")
                        self.assertEqual("denim_timestamp" in fork.manifest, name == "after scheduling")
                        with self.assertRaisesRegex(RuntimeError, "initialization is incomplete"):
                            fork.start()
                        dump.unlink()
                        commands = len(runtime.commands)
                        with self.assertRaisesRegex(RuntimeError, "L1 state is missing"):
                            runtime.retry()
                        self.assertEqual(len(runtime.commands), commands)
                        dump.write_text("{}")
                        runtime.retry()
                    self.assertTrue(devnet.SnapshotFork(fork.directory).initialized())
                    self.assertEqual(fork.manifest["denim_timestamp"], 1294)
                    self.assertEqual(list(fork.manifest["operations"]),
                                     ["seed-upgrades", "seed-version", "set-batcher", "set-signer", "schedule-denim"])
                    self.assertEqual(len(runtime.sent), 5, "committed writes must never be resubmitted")
                    self.assertEqual(runtime.mined, 5)
                    self.assertEqual([call[1] for call in runtime.rpcs].count("anvil_setCode"), 1)
                    self.assertEqual(runtime.contracts[runtime.MOCK]["schedule"], runtime.HISTORY + [1294])

    def test_init_retry_fails_closed_after_ambiguous_submission(self):
        runtime = FakeRuntime(self, self.root / "ambiguous", devnet.ROLES)
        runtime.fail_once(lambda *event: event[:2] == ("eth_sendTransaction", "setUnsafeBlockSigner(address)"),
                          devnet.Unavailable("connection reset"))
        with runtime.patched():
            with self.assertRaises(devnet.Unavailable):
                runtime.fork.prepare_runtime()
            with self.assertRaisesRegex(RuntimeError, "reconcile its nonce"):
                runtime.retry()
            self.assertEqual(runtime.commands[-2:], [("stop", *devnet.ROLES), ("stop", "l1")])
        self.assertEqual([call[1] for call in runtime.rpcs].count("eth_sendTransaction"), 4)
        self.assertEqual(devnet.SnapshotFork(runtime.fork.directory).manifest["phase"], "initializing")

    def test_first_interruption_before_l1_effects_retries_without_a_dump(self):
        for roles in (("sequencer",), devnet.ROLES):
            with self.subTest(roles=roles):
                runtime = FakeRuntime(self, self.root / f"early-{len(roles)}", roles)
                runtime.fail_once(lambda *event: event == ("await_rpc", "l1"), KeyboardInterrupt())
                with runtime.patched():
                    with self.assertRaises(KeyboardInterrupt):
                        runtime.fork.prepare_runtime()
                    self.assertEqual(runtime.commands, [("up", "-d", "--no-build", "l1"), ("stop", *roles), ("stop", "l1")])
                    (runtime.fork.directory / "l1/anvil.json").unlink()  # Anvil never saved state.
                    runtime.retry()
                self.assertTrue(devnet.SnapshotFork(runtime.fork.directory).initialized())

    def test_unsaved_l1_state_after_bootstrap_is_a_failed_initialization(self):
        for stale in (False, True):
            with self.subTest(stale=stale):
                runtime = FakeRuntime(self, self.root / f"unsaved-{stale}", devnet.ROLES)
                runtime.saves_dump = False
                dump = runtime.fork.directory / "l1/anvil.json"
                if stale:
                    dump.parent.mkdir()
                    dump.write_text("{}")  # An earlier periodic dump without the bootstrap writes.
                with runtime.patched():
                    with self.assertRaisesRegex(RuntimeError, "did not save L1 state"):
                        runtime.fork.prepare_runtime()
                    self.assertEqual(runtime.commands[-2:], [("stop", *devnet.ROLES), ("stop", "l1")])
                    stored = devnet.SnapshotFork(runtime.fork.directory)
                    self.assertEqual(stored.manifest["phase"], "initializing")
                    self.assertFalse(stored.initialized())
                    if not stale:
                        with self.assertRaisesRegex(RuntimeError, "L1 state is missing"):
                            runtime.retry()

    def test_completed_init_is_a_noop_even_while_running(self):
        for phase in ("stopped", "starting", "running"):
            with self.subTest(phase=phase):
                self.fork.manifest = completed(phase=phase)
                self.fork.save()
                before = (self.fork.directory / "manifest.json").read_bytes()
                with patch.object(self.fork, "compose") as compose, patch.object(self.fork, "running") as running, \
                        patch.object(devnet, "rpc") as transport, patch("builtins.print"):
                    self.fork.prepare_runtime()
                for effect in (compose, running, transport):
                    effect.assert_not_called()
                self.assertEqual((self.fork.directory / "manifest.json").read_bytes(), before)
        # An earlier launcher's running fork without Denim is stopped before init resumes.
        self.fork.manifest = completed(phase="running", denim_timestamp=None)
        with patch.object(self.fork, "running", return_value=True), \
                patch.object(self.fork, "stop", side_effect=RuntimeError("stopped first")), \
                patch.object(self.fork, "compose") as compose:
            with self.assertRaisesRegex(RuntimeError, "stopped first"):
                self.fork.prepare_runtime()
            compose.assert_not_called()

    @unittest.skipUnless(shutil.which("just"), "requires the just command dispatcher")
    def test_nested_just_commands_forward_arguments_without_starting_services(self):
        for command in ("setup", "download", "init", "up", "down", "start", "stop", "status", "reset",
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
                if command == "verify":
                    for flag in ("--denim", "--restart", "--interrupt"):
                        self.assertNotIn(flag, result.stdout)
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
        devnet.validate_boundary([initial], 19)
        with self.assertRaisesRegex(RuntimeError, "Base mainnet"):
            devnet.validate_boundary([{**initial, "chain_id": 10}], 19)
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
                    devnet.SnapshotFork(self.root / "new").initialize({field: 1})
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

    def test_up_rejects_incomplete_init_with_setup_guidance_before_any_service_operation(self):
        for name, fields in (("metadata only", {"phase": "prepared"}),
                             ("interrupted bootstrap", {"phase": "initializing", "boundary_validated": True}),
                             ("old fork without Denim", {"phase": "stopped", "denim_timestamp": None}),
                             ("old running fork before bootstrap", {"phase": "running", "bootstrapped": False})):
            with self.subTest(name), patch.object(self.fork, "compose") as compose, \
                    patch.object(self.fork, "running") as running, patch.object(devnet, "rpc") as transport:
                self.fork.manifest = completed(**fields)
                with self.assertRaisesRegex(RuntimeError, "initialization is incomplete; rerun just devnet "
                                            f"snapshot setup --dir {self.fork.directory}"):
                    self.fork.start()
                for effect in (compose, running, transport):
                    effect.assert_not_called()

    def test_resume_rejects_missing_l1_dump_before_starting_any_database(self):
        for roles in (("sequencer",), devnet.ROLES):
            with self.subTest(roles=roles):
                self.fork.manifest = completed(datadirs={role: str(self.datadir(f"{role}-{len(roles)}")) for role in roles})
                with patch.object(self.fork, "running", return_value=False), patch.object(self.fork, "endpoint"), \
                        patch.object(self.fork, "compose") as compose:
                    with self.assertRaisesRegex(RuntimeError, "L1 state is missing"):
                        self.fork.start()
                    compose.assert_not_called()
                self.assertEqual(self.fork.manifest["phase"], "stopped")

    def test_start_checks_batcher_rpc_before_sequencing_and_rejects_exited_batcher(self):
        for missing_miner in (True, False):
            with self.subTest(missing_miner=missing_miner):
                self.fork.manifest = completed()
                (self.fork.directory / "l1").mkdir(exist_ok=True)
                (self.fork.directory / "l1/anvil.json").write_text("{}")
                calls = []

                def transport(url, method, *args):
                    calls.append(method)
                    if method.startswith("miner_"):
                        if missing_miner:
                            raise devnet.Unavailable("miner method unavailable")
                        return ["0x0", "0x0"]
                    if method == "admin_sequencerActive":
                        return False
                    if method == "eth_getBlockByNumber":
                        return {"number": "0x7b", "hash": "0x123"}
                    return True

                with self.starting(), \
                        patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)), \
                        patch.object(self.fork, "running_services", return_value={"l1", "sequencer", "validator"}), \
                        patch.object(self.fork, "mine"), \
                        patch.object(devnet, "rpc", side_effect=transport), patch("builtins.print"):
                    with self.assertRaisesRegex(RuntimeError, "miner" if missing_miner else "batcher exited"):
                        self.fork.start()
                self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["phase"], "starting")
                self.assertEqual(calls[-2:], [("stop", "batcher", "sequencer", "validator"), ("stop", "l1")])
                if missing_miner:
                    self.assertNotIn("admin_startSequencer", calls)
                    self.assertNotIn(("up", "-d", "--no-build", "batcher"), calls)

    def test_up_retries_sequencer_start_after_readiness_and_head_race_errors(self):
        self.fork.manifest = completed()
        (self.fork.directory / "l1").mkdir()
        (self.fork.directory / "l1/anvil.json").write_text("{}")
        heads, started = iter(("0xold", "0xnew")), []

        def transport(url, method, *args):
            if method == "eth_getBlockByNumber":
                return {"hash": next(heads)}
            if method == "admin_sequencerActive" and url == "sequencer-cl":
                if not started:
                    started.append("not ready")
                    raise devnet.Unavailable("consensus RPC starting")
                return False
            if method == "admin_startSequencer":
                started.append(args[0])
                if args[0] == "0xold":
                    raise devnet.Unavailable("head moved before admin_startSequencer")
            return method != "admin_sequencerActive"

        with self.starting(), patch.object(self.fork, "mine"), patch.object(devnet.time, "sleep"), \
                patch.object(devnet, "rpc", side_effect=transport), patch("builtins.print"):
            self.fork.start()
        self.assertEqual(started, ["not ready", "0xold", "0xnew"])
        self.assertEqual(self.fork.manifest["phase"], "running")

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
            status = self.fork.status()
            self.assertEqual(status["phase"], "running")
            self.assertEqual(status["rpc_docker_host_only"]["sequencer-ws"], "ws://10.9.0.2:8546")
            transport.assert_not_called()

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

    def test_single_node_start_sequences_and_never_contacts_or_starts_a_validator(self):
        self.fork.manifest = completed()
        del self.fork.manifest["datadirs"]["validator"]
        (self.fork.directory / "l1").mkdir()
        (self.fork.directory / "l1/anvil.json").write_text("{}")
        calls = []

        def node(url, method, *args):
            self.assertNotIn("validator", url)
            calls.append((url, method, *args))
            if method == "eth_getBlockByNumber":
                return {"hash": "0x124", "timestamp": hex(1234)}
            if method == "admin_sequencerActive":
                return False
            return None

        with self.starting(), patch.object(self.fork, "mine"), \
                patch.object(self.fork, "schedule_denim", side_effect=AssertionError("init scheduled Denim")), \
                patch.object(self.fork, "sync_status", side_effect=AssertionError("up must not wait for heads")), \
                patch.object(devnet, "rpc", side_effect=node), patch("builtins.print"):
            self.fork.start()
            self.assertEqual(self.fork.manifest["phase"], "running")
            self.assertIn(("l1", "anvil_setIntervalMining", 12), calls)
            self.assertIn(("sequencer-cl", "admin_startSequencer", "0x124"), calls)
            self.assertEqual([c.args for c in self.fork.compose.call_args_list], [
                ("up", "-d", "--no-build", "l1"), ("up", "-d", "--no-build", "sequencer"),
                ("up", "-d", "--no-build", "batcher")])
        with patch.object(self.fork, "running", return_value=True), patch.object(self.fork, "compose") as compose, \
                patch.object(self.fork, "url", side_effect=lambda role: role), \
                patch.object(self.fork, "sync_status", return_value=sync_status(102, safe=124)), \
                patch.object(devnet, "rpc", side_effect=node):
            self.fork.peers()
            self.fork.stop()
            self.assertNotIn("validator", str(compose.call_args_list))
        with patch.object(self.fork, "containers", return_value=[container(s) for s in ("l1", "sequencer", "batcher")]):
            self.fork.manifest["phase"] = "running"
            status = self.fork.status()
            self.assertEqual(status["phase"], "running")
            self.assertNotIn("validator", status["rpc_docker_host_only"])

    def test_fast_denim_uses_only_local_mock_and_preserves_history_on_retry(self):
        history = list(range(100, 113))
        address = "0x" + "4" * 40
        self.fork.manifest.update(phase="running", fast_denim=True, local_protocol_versions=address, schedule=history)
        schedule = list(history)
        sent = []

        def call(url, contract, signature, *args):
            self.assertEqual(contract, address)
            if signature == "getSchedule()(uint64[])":
                return schedule[:]
            self.assertEqual(signature, "minimumProtocolVersion()(uint256)")
            return 42

        def send(name, sender, contract, signature, values):
            self.assertEqual((name, contract, signature), ("schedule-denim", address, "setSchedule(uint64[])"))
            schedule[:] = json.loads(values)
            sent.append(values)
            self.fork.manifest["operations"][name] = {"hash": "0xsent"}

        def rpc(url, method, *args):
            if method == "eth_getTransactionReceipt":
                return {"status": "0x1"}
            return {"timestamp": hex(1003 if url == "sequencer" else 1001)}

        with patch.object(self.fork, "assert_local_l1"), patch.object(self.fork, "wait_upgrades"), \
                patch.object(self.fork, "url", side_effect=lambda role: role), patch.object(self.fork, "send", side_effect=send), \
                patch.object(devnet, "call", side_effect=call), patch.object(devnet, "rpc", side_effect=rpc), \
                patch.object(devnet.time, "time", return_value=1002), patch("builtins.print"):
            self.fork.schedule_denim()
            self.fork.schedule_denim()
            with self.assertRaisesRegex(RuntimeError, "refusing to move"):
                self.fork.schedule_denim(1066)
        self.assertEqual(schedule, history + [1064])
        self.assertEqual(len(sent), 1)
        self.assertEqual(self.fork.manifest["denim_timestamp"], 1064)

    def test_denim_scheduling_requires_initialization_or_a_running_fork(self):
        for phase in ("prepared", "stopped", "starting"):
            with self.subTest(phase=phase), self.protocol_versions([]) as fake:
                self.fork.manifest["phase"] = phase
                with self.assertRaisesRegex(RuntimeError, "initialize or start the fork"):
                    self.fork.schedule_denim()
                self.assertEqual(fake["sent"], [])
                self.assertNotIn("pending_denim_timestamp", self.fork.manifest)

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

    def test_stop_during_incomplete_init_keeps_the_init_phase(self):
        for phase in ("prepared", "initializing"):
            with self.subTest(phase=phase), patch.object(self.fork, "running", return_value=False):
                self.fork.manifest["phase"] = phase
                self.fork.stop()
                self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["phase"], phase)

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
            self.assertEqual(self.fork.url("sequencer-ws"), "ws://10.9.0.7:8546")

    def test_unavailable_role_is_retryable_but_ambiguous_identity_is_not(self):
        cases = {
            "stopped": ([container("sequencer", running=False)], "sequencer", devnet.Unavailable),
            "inspection has no consensus RPC": ([container("inspect-sequencer")], "sequencer-cl", devnet.Unavailable),
            "inspection has no WebSocket RPC": ([container("inspect-sequencer")], "sequencer-ws", devnet.Unavailable),
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

    def test_running_refreshes_container_state_without_compose_or_keys(self):
        self.fork._containers = []
        with patch.object(devnet, "run", side_effect=[
                "abc", json.dumps([container("inspect-validator")]),
                "abc", json.dumps([container("inspect-validator", running=False)]),
        ]):
            self.assertTrue(self.fork.running())
            self.assertFalse(self.fork.running())

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

    def test_boundary_gate_resume_rejects_a_different_recorded_successor(self):
        mine = self.boundary_gate(101)
        self.fork.manifest["boundary"] = {"l1_successor": {"number": "0x65", "hash": "0xearlier"}}
        with self.assertRaisesRegex(RuntimeError, "successor of F changed"):
            self.fork.wait_boundary()
        mine.assert_not_called()
        self.assertFalse(self.fork.manifest.get("boundary_validated"))

    def test_boundary_gate_rejects_reorged_snapshot_head_without_persisting(self):
        self.boundary_gate(100, canonical="0xother")
        with self.assertRaisesRegex(RuntimeError, "no longer canonical"):
            self.fork.wait_boundary()
        stored = devnet.SnapshotFork(self.fork.directory).manifest
        self.assertFalse(stored.get("boundary_validated"))
        # The mined successor is journaled, so a retry requires the saved L1 state.
        self.assertEqual(stored["boundary"], {"l1_successor": {"number": "0x65", "hash": "0xsuccessor"}})

    def test_execution_rpc_wait_has_no_deadline_and_keeps_reporting(self):
        self.fork.timeout = 60
        for role, service in (("sequencer", "inspect-sequencer"), ("validator", "validator")):
            with self.subTest(role=role):
                attempts = 0

                def request(*args):
                    nonlocal attempts
                    attempts += 1
                    self.assertTrue(output.called, "announce the wait before the first RPC request")
                    if attempts == 4:
                        self.assertGreaterEqual(output.call_count, 2, "report again during a long wait")
                        return "0x2105"
                    raise devnet.Unavailable("provider-secret")

                with patch.object(self.fork, "url", return_value="https://rpc.invalid/provider-secret"), \
                        patch.object(devnet, "rpc", side_effect=request), \
                        patch.object(self.fork, "containers", return_value=[container(service)]), \
                        patch.object(self.fork, "rpc_startup_status", return_value=f"{service}: repairing history indexes"), \
                        patch.object(devnet.time, "monotonic", side_effect=[0, 1, 31, 10801]), \
                        patch.object(devnet.time, "sleep"), patch("builtins.print") as output:
                    self.fork.await_rpc(role)
                    self.assertEqual(attempts, 4)
                    self.assertIn(f"{role} execution RPC", str(output.call_args_list))
                    self.assertIn(f"{service}: repairing history indexes", str(output.call_args_list))
                    self.assertIn("10801s elapsed", str(output.call_args_list))
                    self.assertNotIn("remaining", str(output.call_args_list))
                    self.assertNotIn("provider-secret", str(output.call_args_list))
                    self.assertTrue(all(call.kwargs.get("flush") for call in output.call_args_list))
                    self.assertTrue(all(call.kwargs.get("file") is sys.stderr for call in output.call_args_list))

    def test_execution_rpc_wait_fails_if_container_exits_or_disappears(self):
        for service in ("inspect-sequencer", "sequencer"):
            for records in ([container(service, running=False)], []):
                with self.subTest(service=service, records=records):
                    # A previously cached running container must not hide its exit.
                    self.fork._containers = [container(service)]
                    with patch.object(devnet, "run", side_effect=["container-id" if records else "", json.dumps(records)]), \
                            patch.object(devnet, "rpc", side_effect=devnet.Unavailable("provider-secret")), \
                            patch.object(devnet.time, "monotonic", side_effect=[0, 1]), \
                            patch.object(devnet.time, "sleep") as sleep, patch("builtins.print"):
                        with self.assertRaisesRegex(RuntimeError, "sequencer execution container exited or is missing"):
                            self.fork.await_rpc("sequencer")
                        sleep.assert_not_called()

    def test_l1_rpc_wait_keeps_its_deadline(self):
        self.fork.timeout = 60
        with patch.object(self.fork, "containers", return_value=[container("l1")]), \
                patch.object(devnet, "rpc", side_effect=devnet.Unavailable("provider-secret")), \
                patch.object(devnet.time, "monotonic", side_effect=[0, 1, 61]), \
                patch.object(devnet.time, "sleep"), patch("builtins.print") as output:
            with self.assertRaisesRegex(RuntimeError, "timed out: l1 execution RPC"):
                self.fork.await_rpc("l1")
            self.assertIn("59s remaining", str(output.call_args_list))

    def test_rpc_startup_status_reports_current_container_history_work_without_raw_logs(self):
        record = container("inspect-sequencer")
        record["Id"] = "sequencer-container"
        record["State"]["StartedAt"] = "2026-10-05T22:51:20Z"
        logs = (
            "2026-10-05T22:51:24Z INFO StoragesHistory: healing via changesets checkpoint=50945326\n"
            "2026-10-05T22:56:03Z INFO StoragesHistory: unwinding batch "
            "\x1b[3mbatch_num\x1b[0m=8 total_batches=124 batch_start=51015327 batch_end=51025326 "
            "upstream=https://secret.invalid/key token=provider-secret\n"
            "unrelated log with another-secret\n")
        with patch.object(self.fork, "containers", return_value=[container("validator"), record]) as containers, \
                patch.object(devnet.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, stdout=logs)) as read:
            message = self.fork.rpc_startup_status("sequencer")
            self.assertIn("inspect-sequencer", message)
            self.assertIn("repairing storage-history indexes", message)
            self.assertIn("batch 8/124", message)
            self.assertIn("51015327", message)
            self.assertIn("51025326", message)
            self.assertIn("2026-10-05T22:56:03Z", message)
            self.assertNotIn("secret", message)
            self.assertNotIn("\x1b", message)
            args = read.call_args.args[0]
            self.assertEqual(args[args.index("--since") + 1], record["State"]["StartedAt"])
            self.assertIn(record["Id"], args)
            self.assertEqual(read.call_args.kwargs["stderr"], subprocess.STDOUT)
            self.assertLessEqual(read.call_args.kwargs["timeout"], 5)

            containers.return_value.append(container("sequencer", running=False))
            self.assertIn("batch 8/124", self.fork.rpc_startup_status("sequencer"))
            containers.return_value.pop()
            read.return_value.stdout += "2026-10-05T22:57:00Z INFO Collecting indices processed_blocks=21385 current_block=50966711\n"
            message = self.fork.rpc_startup_status("sequencer")
            self.assertIn("rebuilding history indexes", message)
            self.assertIn("50966711", message)
            self.assertNotIn("batch 8/124", message)

            read.return_value.stdout = "unrecognized output with provider-secret\n"
            self.assertIn("no recognized startup progress", self.fork.rpc_startup_status("sequencer"))
            read.side_effect = subprocess.TimeoutExpired("docker", 5)
            self.assertIn("logs unavailable", self.fork.rpc_startup_status("sequencer"))
            record["State"].update(Running=False, ExitCode=137)
            read.reset_mock()
            self.assertIn("exited (code 137)", self.fork.rpc_startup_status("sequencer"))
            read.assert_not_called()

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
            self.assertNotIn("--p2p.no-discovery", command)
            self.assertIn("--no-persist-peers", command)
            self.assertIn("--l1-slot-duration-override=12", command)
            self.assertNotIn("--rollup.sequencer", " ".join(command))
            self.assertIn("sequencer", command)
            self.assertIn("--sequencer.stopped", command)
        # The batcher's default throttling calls miner_setMaxDASize on the sequencer's HTTP RPC.
        sequencer_command = config["services"]["sequencer"]["command"]
        http_apis = next(arg.split("=", 1)[1].split(",") for arg in sequencer_command if arg.startswith("--http.api="))
        self.assertEqual(set(http_apis), {"eth", "net", "web3", "debug", "trace", "miner"})
        for flag in ("--ws", "--ws.addr=0.0.0.0", "--ws.port=8546", "--ws.api=eth,net,web3,debug,trace"):
            self.assertIn(flag, sequencer_command)
        self.assertEqual(config["services"]["batcher"]["stop_signal"], "SIGTERM")
        del self.fork.manifest["datadirs"]["validator"]
        single = json.loads(self.fork.compose("config", "--format", "json"))
        self.assertEqual(set(single["services"]), {"l1", "sequencer", "batcher"})
        for service in single["services"].values():
            self.assertFalse(any("--full" in arg or "--prune." in arg for arg in service["command"]))
        with self.assertRaisesRegex(RuntimeError, "no validator datadir"):
            self.fork.compose("up", "inspect-validator")


class QualificationTests(unittest.TestCase):
    def test_default_verify_exercises_denim_clean_restart_and_interrupted_recovery(self):
        before, window = self.window()
        headers = {124: before, **dict(enumerate(window, 125))}
        for height in (123, 132, 133, 134, 135):
            headers[height] = {"hash": str(height), "timestamp": hex(990 if height == 123 else 1002)}
        for height, header in headers.items():
            header.update(number=hex(height), stateRoot=f"root-{height}")
        for missing_window_transaction in (False, True):
            with self.subTest(missing_window_transaction=missing_window_transaction), tempfile.TemporaryDirectory() as directory:
                fork = Mock(spec=devnet.SnapshotFork)
                fork.roles = devnet.ROLES
                fork.directory, fork.timeout = Path(directory), 1
                fork.manifest = {**manifest(), "phase": "running", "denim_timestamp": 1000,
                                 "initial": [{"latest": {"block_info": {"number": 123}},
                                              "rollup_config": {"genesis": {"l2_time": 0}, "block_time": 2}}]}
                fork.url.side_effect = lambda role: role
                fork.sync_status.return_value = {"unsafe_l2": {"l1origin": {"number": 10}},
                                                 "safe_l2": {"number": 122, "l1origin": {"number": 100}}}
                receipts = [{"blockNumber": hex(height), "blockHash": header["hash"]}
                            for height, header in sorted(headers.items())
                            if not (missing_window_transaction and height == 127)]

                def rpc(url, method, *args):
                    if method == "admin_sequencerActive":
                        return False
                    if method == "eth_getBalance":
                        return hex(10**18)
                    if method == "eth_blockNumber":
                        return hex(150 if url == "l1" else 132)
                    if method == "eth_getBlockByNumber":
                        return {"timestamp": hex(900)} if args[0] == "latest" else headers[int(args[0], 16)]
                    self.fail(f"unexpected RPC: {method}")

                with patch.object(verification, "rpc", side_effect=rpc), \
                        patch.object(verification, "transact", side_effect=receipts), \
                        patch.object(verification, "derived", side_effect=lambda f, h: headers[h]) as derived, \
                        patch.object(verification, "local_blobs", return_value=[{"path": "blob"}]), \
                        patch.object(verification, "wait_denim_window") as denim, \
                        patch.object(verification, "assert_retained") as retained, patch("builtins.print"):
                    if missing_window_transaction:
                        with self.assertRaisesRegex(RuntimeError, "missed an activation-window block"):
                            verification.verify(fork)
                        self.assertFalse((fork.directory / "verification.json").exists())
                        continue
                    verification.verify(fork)
                report = json.loads((fork.directory / "verification.json").read_text())
                self.assertEqual(report["receipts"], receipts)
                self.assertEqual(report["activation_block_timestamp"], 1000)
                self.assertEqual(report["blobs"], [{"path": "blob"}])
                self.assertEqual({c.args[1] for c in derived.call_args_list},
                                 {123, 124, 125, 126, 127, 128, 129, 130, 131, 133, 134, 135})
                denim.assert_called_once_with(fork, 1000)
                self.assertEqual(fork.start.call_count, 4)
                self.assertEqual(fork.stop.call_count, 3)
                self.assertEqual(retained.call_count, 4)
                self.assertEqual([c.args for c in fork.compose.call_args_list], [
                    ("stop", "batcher"), ("stop", "batcher"),
                    ("kill", "--signal", "SIGKILL", "sequencer", "validator"), ("stop", "l1")])
                fork.peers.assert_not_called()

    def test_verify_rejects_missed_denim_window_before_deposit_or_restart(self):
        fork = Mock(spec=devnet.SnapshotFork)
        fork.roles = devnet.ROLES
        fork.manifest = {"phase": "running", "denim_timestamp": 1000,
                         "initial": [{"rollup_config": {"genesis": {"l2_time": 0}, "block_time": 2}}]}
        fork.deposit.side_effect = AssertionError("must check activation before depositing")
        for timestamp in (998, 1005):
            with self.subTest(timestamp=timestamp), \
                    patch.object(verification, "rpc", side_effect=[False, {"timestamp": hex(timestamp)}]):
                with self.assertRaisesRegex(RuntimeError, "before Denim activation"):
                    verification.verify(fork)
        fork.deposit.assert_not_called()
        fork.stop.assert_not_called()

    def test_derivation_waits_for_safe_head_even_when_unsafe_has_advanced(self):
        fork = Mock(spec=devnet.SnapshotFork)
        fork.roles = devnet.ROLES
        fork.timeout = 1
        fork.url.side_effect = lambda role: role
        fork.sync_status.side_effect = [
            {"safe_l2": {"number": 123}, "unsafe_l2": {"number": 130}},
            {"safe_l2": {"number": 124}, "unsafe_l2": {"number": 130}},
        ]
        header = {"number": "0x7c", "hash": "canonical", "stateRoot": "root"}

        def rpc(url, method, height, full):
            self.assertEqual(fork.sync_status.call_count, 2, "unsafe gossip is not proof of derivation")
            self.assertEqual((method, height, full), ("eth_getBlockByNumber", "0x7c", False))
            return header

        with patch.object(verification, "rpc", side_effect=rpc), \
                patch.object(devnet.time, "sleep"), patch("builtins.print"):
            self.assertEqual(verification.derived(fork, 124), header)

    def test_retention_verification_waits_for_recovery_then_rejects_changed_blocks(self):
        for changed in (False, True):
            with self.subTest(changed=changed):
                fork = Mock(spec=devnet.SnapshotFork)
                fork.roles, fork.timeout = devnet.ROLES, 1
                fork.url.side_effect = lambda role: role
                heights = {"sequencer": iter((123, 125)), "validator": iter((124, 125))}
                recovered = set()

                def status(role):
                    height = next(heights[role])
                    if height == 125:
                        recovered.add(role)
                    return {"safe_l2": {"number": height}}

                fork.sync_status.side_effect = status
                saved = {"number": "0x7d", "hash": "canonical", "stateRoot": "root"}

                def rpc(url, *args):
                    self.assertEqual(recovered, set(devnet.ROLES))
                    return {**saved, "hash": "conflict"} if changed and url == "validator" else saved

                with patch.object(verification, "rpc", side_effect=rpc), \
                        patch.object(devnet.time, "sleep"), patch("builtins.print"):
                    report = {"blocks": [saved], "blobs": [], "receipts": []}
                    if changed:
                        with self.assertRaisesRegex(RuntimeError, "changed across restart"):
                            verification.assert_retained(fork, report)
                    else:
                        verification.assert_retained(fork, report)

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
        command = [sys.executable, str(Path(verification.__file__)), "--dir", os.environ["BASE_SNAPSHOT_FORK_DIR"]]
        subprocess.run(command, check=True)


@unittest.skipUnless(os.environ.get("BASE_SNAPSHOT_TEST_ANVIL"), "opt-in disposable local Anvil contract test")
class FastDenimContractTests(unittest.TestCase):
    def test_mock_seed_schedule_and_restore_use_real_transactions(self):
        """Real EVM and persisted state; no production upstream or L2 node is used."""
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        with tempfile.TemporaryDirectory() as directory, subprocess.Popen(
                ["anvil", "--silent", "--host", "127.0.0.1", "--port", str(port), "--no-mining", "--chain-id", "1"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as process:
            try:
                url = f"http://127.0.0.1:{port}"

                def ready():
                    self.assertIsNone(process.poll(), "disposable Anvil exited")
                    try:
                        return devnet.rpc(url, "eth_chainId") == "0x1"
                    except devnet.Unavailable:
                        return False

                devnet.wait("disposable Anvil", ready, 10, poll_interval=0.1)
                fork = devnet.SnapshotFork(directory, timeout=10)
                code = devnet.run("forge", "inspect", "--root", str(devnet.ROOT / "crates/utilities/test-utils/contracts"),
                                  "src/MockProtocolVersions.sol:MockProtocolVersions", "deployedBytecode")
                history = list(range(100, 113))
                # Init seeds and schedules with interval mining paused; send() mines each write.
                fork.manifest = {**manifest(), "phase": "initializing", "port": port, "fast_denim": True,
                                 "local_protocol_versions": "0x" + "4" * 40, "upgrade_code": code,
                                 "minimum_protocol_version": 42, "schedule": history}
                sender, original = fork.manifest["accounts"]["user"], fork.manifest["protocol_versions"]
                # Local Anvil stands in for both timestamp RPCs; L2 observation is checked separately.
                with patch.object(fork, "assert_local_l1"), patch.object(fork, "url", return_value=url), \
                        patch.object(fork, "mine", side_effect=lambda: devnet.rpc(url, "evm_mine")), \
                        patch.object(fork, "wait_upgrades"):
                    devnet.rpc(url, "anvil_setCode", original, code)
                    fork.send("fixture-history", sender, original, "setSchedule(uint64[])", json.dumps(history))
                    fork.send("fixture-version", sender, original, "setMinimumProtocolVersion(uint256)", 42)
                    send = fork.send

                    def interrupted(name, *args, **kwargs):
                        if name == "seed-version":
                            raise RuntimeError("interrupted seeding")
                        return send(name, *args, **kwargs)

                    with patch.object(fork, "send", side_effect=interrupted), self.assertRaisesRegex(RuntimeError, "interrupted"):
                        fork.seed_upgrade_signal()
                    nonce = devnet.number(devnet.rpc(url, "eth_getTransactionCount", sender, "latest"))
                    fork.seed_upgrade_signal()
                    self.assertEqual(devnet.number(devnet.rpc(url, "eth_getTransactionCount", sender, "latest")), nonce + 1)
                    fork.seed_upgrade_signal()
                    fork.schedule_denim()
                    timestamp = fork.manifest["denim_timestamp"]
                    self.assertEqual(devnet.call(url, fork.upgrade_contract, "getSchedule()(uint64[])"), history + [timestamp])
                    self.assertEqual(devnet.call(url, original, "getSchedule()(uint64[])"), history)
                    receipt = fork.manifest["operations"]["schedule-denim"]["receipt"]
                    self.assertEqual(devnet.number(receipt["status"]), 1)
                    included = devnet.rpc(url, "eth_getBlockByNumber", receipt["blockNumber"], False)
                    self.assertLess(timestamp - devnet.number(included["timestamp"]), 90)
                    nonce = devnet.rpc(url, "eth_getTransactionCount", sender, "latest")
                    state = devnet.rpc(url, "anvil_dumpState")
                    devnet.rpc(url, "anvil_reset")
                    devnet.rpc(url, "anvil_loadState", state)
                    fork.manifest = devnet.SnapshotFork(directory).manifest
                    fork.seed_upgrade_signal()
                    fork.validate_restored_contracts()
                    fork.schedule_denim()
                    self.assertEqual(fork.manifest["denim_timestamp"], timestamp)
                    self.assertEqual(devnet.rpc(url, "eth_getTransactionCount", sender, "latest"), nonce)
                    fork.send("tamper-version", sender, fork.upgrade_contract, "setMinimumProtocolVersion(uint256)", 43)
                    with self.assertRaisesRegex(RuntimeError, "minimum protocol version differs"):
                        fork.validate_restored_contracts()
            finally:
                process.terminate()
                process.wait(timeout=10)


if __name__ == "__main__":
    unittest.main()
