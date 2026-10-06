#!/usr/bin/env python3
"""Offline launcher tests."""

import contextlib
import copy
import fcntl
import io
import json
import os
from pathlib import Path
import re
import shutil
import socket
import stat
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
            "finalized_l2": {"number": safe, "hash": safe_hash},
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

    def preparation(self, suffix="", **changes):
        """Init input with fresh datadirs and a free L1 port rather than the default, which may be in use."""
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        config = {"sequencer_datadir": str(self.datadir("builder" + suffix)),
                  "validator_datadir": str(self.datadir("validator" + suffix)),
                  "port": port, **{role + "_image": role + ":local" for role in ("base", "anvil", "batcher")}}
        return {**config, **changes}

    def kill_while_writing(self, path, write_json=devnet.write_json):
        """Leaves what write_json leaves when its process is killed after writing, before the rename."""
        child = os.fork()
        if child == 0:
            with patch.object(devnet.os, "fsync", side_effect=lambda _: os._exit(9)):
                write_json(path, {"interrupted": True})
            os._exit(1)
        self.assertEqual(os.waitstatus_to_exitcode(os.waitpid(child, 0)[1]), 9)
        self.assertFalse(path.exists())

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

    def test_response_without_result_is_unavailable_but_null_result_is_returned(self):
        for envelope in (None, 42, ["result"], "result", {},
                         {"jsonrpc": "2.0", "id": 1, "message": "secret-api-key"}):
            with patch.object(devnet, "request_json", return_value=envelope):
                with self.assertRaises(devnet.Unavailable) as caught:
                    devnet.rpc("https://secret.invalid/key", "eth_chainId", upstream=True)
            self.assertNotIn("secret-api-key", str(caught.exception))
            self.assertNotIn("secret.invalid", str(caught.exception))
        with patch.object(devnet, "request_json", return_value={"jsonrpc": "2.0", "id": 1, "result": None}):
            self.assertIsNone(devnet.rpc("https://secret.invalid/key", "eth_getBlockByNumber", upstream=True))

    def test_malformed_endpoint_failure_does_not_print_secret(self):
        # http.client rejects the space before connecting, so the request stays offline.
        endpoint = "http://127.0.0.1:1/secret-key path"
        for request in (lambda: devnet.request_json(endpoint), lambda: devnet.rpc(endpoint, "eth_chainId")):
            with self.assertRaises(devnet.Unavailable) as caught:
                request()
            self.assertEqual(str(caught.exception), "RPC/Beacon request failed (endpoint redacted)")

    def test_endpoint_keeps_a_credential_ending_in_a_slash(self):
        with patch.dict(os.environ, {"TEST_EXECUTION_URL": "https://rpc.invalid/v2?key=abc/"}):
            self.assertEqual(self.fork.endpoint("execution"), "https://rpc.invalid/v2?key=abc/")

    def test_concurrent_json_writers_never_share_a_temporary_file(self):
        # Setups selecting different forks hold different locks but write the same global selection.
        path, fsync = self.root / "selection.json", os.fsync
        first, second = {"directory": "/first/" + "x" * 4096}, {"directory": "/second"}

        def interleave(descriptor):
            fsync(descriptor)
            if not interleave.done:
                interleave.done = True
                devnet.write_json(path, second)  # Runs entirely between the first writer's fsync and rename.

        interleave.done = False
        with patch.object(devnet.os, "fsync", side_effect=interleave):
            devnet.write_json(path, first)
        self.assertTrue(interleave.done)
        self.assertEqual(json.loads(path.read_text()), first)
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertEqual([entry.name for entry in self.root.iterdir() if entry.name.startswith("selection")],
                         ["selection.json"])

    def test_failed_json_write_removes_its_temporary_file_and_keeps_the_previous_file(self):
        path = self.root / "selection.json"
        devnet.write_json(path, {"directory": "/previous"})
        with patch.object(devnet.json, "dump", side_effect=OSError("disk full")), self.assertRaises(OSError):
            devnet.write_json(path, {"directory": "/next"})
        self.assertEqual(json.loads(path.read_text()), {"directory": "/previous"})
        self.assertEqual([entry.name for entry in self.root.iterdir() if entry.name.startswith("selection")],
                         ["selection.json"])

    def test_beacon_paths_extend_the_endpoint_path_before_its_query(self):
        requests = []
        opener = Mock()
        opener.open.side_effect = lambda request, timeout: requests.append(request) or io.BytesIO(
            b'{"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}')
        with patch.object(devnet.urllib.request, "build_opener", return_value=opener):
            with patch.object(devnet, "run", side_effect=lambda *args, **_: "sha256:" + "d" * 64 if args[0] == "docker"
                              else "0x" + args[-1][-40:]), \
                    patch.dict(os.environ, {"TEST_EXECUTION_URL": "https://rpc.invalid/key",
                                            "TEST_BEACON_URL": "https://beacon.invalid/path?token=secret"}), \
                    patch.object(devnet, "rpc", return_value="0x1"), \
                    patch.object(devnet.SnapshotFork, "inspect", side_effect=KeyboardInterrupt()), \
                    patch("builtins.print"):
                fork = devnet.SnapshotFork(self.root / "new")
                fork.directory.mkdir()
                with self.assertRaises(KeyboardInterrupt):
                    fork.initialize(self.preparation(execution_env="TEST_EXECUTION_URL", beacon_env="TEST_BEACON_URL"),
                                    allow_write=True)
            self.assertEqual([request.full_url for request in requests],
                             ["https://beacon.invalid/path/eth/v1/beacon/genesis?token=secret",
                              "https://beacon.invalid/path/eth/v1/config/spec?token=secret"])
            self.assertTrue(all(request.data is None for request in requests))
            self.assertEqual(devnet.SnapshotFork(fork.directory).manifest["beacon_genesis"],
                             {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"})

            spec = "/eth/v1/config/spec"
            for endpoint, expected in (
                    ("https://beacon.invalid/path/?token=secret",
                     "https://beacon.invalid/path/eth/v1/config/spec?token=secret"),
                    ("http://127.0.0.1:19545", "http://127.0.0.1:19545/eth/v1/config/spec")):
                devnet.request_json(endpoint, path=spec)
                self.assertEqual(requests[-1].full_url, expected)
            # JSON-RPC bodies still go to the endpoint exactly as configured.
            devnet.request_json("https://rpc.invalid/key?token=secret", {"method": "eth_chainId"})
            self.assertEqual(requests[-1].full_url, "https://rpc.invalid/key?token=secret")
            self.assertEqual(json.loads(requests[-1].data), {"method": "eth_chainId"})

            opener.open.side_effect = devnet.urllib.error.URLError("https://beacon.invalid/path?token=secret")
            with self.assertRaises(devnet.Unavailable) as caught:
                devnet.request_json("https://beacon.invalid/path?token=secret", path=spec)
            self.assertNotIn("secret", str(caught.exception))

    def test_l1_url_uses_published_loopback_port_without_docker_lookup(self):
        with patch.object(devnet, "run") as run:
            self.assertEqual(self.fork.url("l1"), "http://127.0.0.1:19545")
            run.assert_not_called()

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

    def test_discovery_runs_only_on_sequencer_with_normalized_credentials(self):
        self.fork.manifest["upstreams"] = {"execution": "CUSTOM_EXECUTION", "beacon": "CUSTOM_BEACON"}
        calls = []
        def inspector(*args, env=None, timeout=None, secrets=None):
            if "--help" in args:
                return ""
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
                patch.object(devnet, "run", side_effect=inspector), patch("builtins.print"):
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

    def test_failed_discovery_stops_inspection_nodes(self):
        environment = {"TEST_EXECUTION_URL": "https://e.invalid", "TEST_BEACON_URL": "https://b.invalid"}
        for output, pattern in ((json.dumps(snapshot()), "fork"), (RuntimeError("base-devnet failed"), "base-devnet")):
            with self.subTest(pattern=pattern), patch.dict(os.environ, environment), \
                    patch.object(self.fork, "compose") as compose, patch.object(self.fork, "await_rpc"), \
                    patch.object(self.fork, "containers", return_value=[]), \
                    patch.object(self.fork, "url", return_value="http://10.9.0.2:8545"), \
                    patch.object(devnet, "run", side_effect=["", output]), patch("builtins.print"):
                with self.assertRaisesRegex(RuntimeError, pattern):
                    self.fork.inspect(discover=True)
                self.assertEqual(compose.call_args.args,
                                 ("--profile", "inspect", "stop", "inspect-sequencer", "inspect-validator"))

    def test_inspection_refuses_datadirs_held_by_production_nodes(self):
        with patch.object(self.fork, "containers", return_value=[container("validator")]), \
                patch.object(self.fork, "compose") as compose:
            with self.assertRaisesRegex(RuntimeError, "production"):
                self.fork.inspect()
            compose.assert_not_called()

    def test_missing_or_incompatible_inspector_fails_before_starting_containers(self):
        incompatible = self.root / "old-base-devnet"
        incompatible.write_text("#!/bin/sh\nexit 2\n")
        incompatible.chmod(0o700)
        for inspector in (self.root / "missing", incompatible):
            with self.subTest(inspector=inspector.name), \
                    patch.dict(os.environ, {"BASE_SNAPSHOT_INSPECTOR": str(inspector)}), \
                    patch.object(self.fork, "containers", return_value=[]), \
                    patch.object(self.fork, "compose") as compose:
                with self.assertRaisesRegex(RuntimeError, "cargo build .*base-devnet.* BASE_SNAPSHOT_INSPECTOR"):
                    self.fork.inspect()
                compose.assert_not_called()

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
                        patch.object(devnet.time, "sleep") as sleep, patch("builtins.print") as output:
                    self.fork.await_rpc(role)
                    self.assertEqual(attempts, 4)
                    self.assertEqual([call.args for call in sleep.call_args_list], [(5,)] * 3)
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
        with patch.object(self.fork, "compose_env", return_value={"SNAPSHOT_BASE_IMAGE": "provider-secret"}), \
                patch.object(devnet, "run", return_value="raw-output-secret") as run, \
                patch("builtins.print") as output:
            self.fork.compose("--profile", "inspect", "up", "-d", "--no-build", "inspect-sequencer")
            run.assert_called_once()
            self.assertTrue(output.called, "announce container startup instead of silently capturing Compose")
            self.assertIn("inspect-sequencer", str(output.call_args_list))
            self.assertNotIn("secret", str(output.call_args_list))
            self.assertTrue(all(call.kwargs.get("flush") for call in output.call_args_list))
            self.assertTrue(all(call.kwargs.get("file") is sys.stderr for call in output.call_args_list))

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
        for role in devnet.ROLES:
            # Inspection nodes are offline and share each production node's datadir.
            inspection = config["services"]["inspect-" + role]
            self.assertIn("--disable-discovery", inspection["command"])
            self.assertIn({"source": self.fork.manifest["datadirs"][role], "target": "/data"},
                          [{key: volume[key] for key in ("source", "target")} for volume in inspection["volumes"]])
            command = config["services"][role]["command"]
            self.assertNotIn("--p2p.no-discovery", command)
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

    def test_init_retry_preserves_identity_and_keys_and_completed_init_is_a_noop(self):
        config = self.preparation()
        config_path = self.root / "input.json"
        devnet.write_json(config_path, config)
        # Setup interrupted before initialization leaves only its private endpoints.
        devnet.write_json(self.fork.directory / "upstreams.json", {"execution": "https://rpc.invalid/key"})
        initial = snapshot()
        initial["rollup_config"]["l1_system_config_address"] = "0x" + "1" * 40
        discovered = {"number": 100, "hash": "0xf", "timestamp": 1240, "parentHash": "0xe"}
        header = {"number": "0x64", "hash": "0xf", "timestamp": "0x4d8", "parentHash": "0xe"}
        minimum = iter([0, 1])

        def transport(url, method, *args, **kwargs):
            self.assertTrue(kwargs["upstream"])
            if method == "eth_chainId":
                return "0x1"
            if method == "eth_getBlockByNumber":
                return {"number": "0x13", "hash": "0x19"} if args[0] == "0x13" else header
            return "0x01"

        def contract(url, address, signature, *args, **kwargs):
            if signature == "getSchedule()(uint64[])":
                return [0] * 14
            if signature == "minimumProtocolVersion()(uint256)":
                return next(minimum)
            return "0x" + "2" * 40

        def inspection():
            return [{**copy.deepcopy(initial), "fork": discovered}, copy.deepcopy(initial)]

        with patch.object(sys, "argv", ["launcher", "init", "--dir", str(self.fork.directory),
                                       "--config", str(config_path), "--allow-write"]), \
                patch.object(devnet, "run", side_effect=lambda *args, **_: "sha256:" + "d" * 64 if args[0] == "docker"
                             else "0x" + args[-1][-40:]), \
                patch.dict(os.environ, {"SNAPSHOT_UPSTREAM_EXECUTION": "https://rpc.invalid/key",
                                        "SNAPSHOT_UPSTREAM_BEACON": "https://rpc.invalid/key"}), \
                patch.object(devnet, "rpc", side_effect=transport), \
                patch.object(devnet, "call", side_effect=contract), \
                patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                patch.object(devnet.SnapshotFork, "inspect",
                             side_effect=[KeyboardInterrupt(), inspection(), inspection()]) as inspect, \
                patch("builtins.print"):
            with self.assertRaises(KeyboardInterrupt):
                devnet.main()
            before = devnet.SnapshotFork(self.fork.directory).manifest
            keys = (self.fork.directory / "keys.json").read_bytes()
            self.assertEqual(before["phase"], "inspecting")
            with self.assertRaisesRegex(RuntimeError, "nonzero minimum protocol version"):
                devnet.main()
            self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["phase"], "inspecting")
            self.assertFalse((self.fork.directory / "config/rollup.json").exists())
            devnet.main()
            after = devnet.SnapshotFork(self.fork.directory).manifest
            self.assertEqual(after["phase"], "prepared")
            self.assertEqual(after["fork"], header)
            self.assertEqual(after["project"], before["project"])
            self.assertEqual(after["accounts"], before["accounts"])
            self.assertEqual((self.fork.directory / "keys.json").read_bytes(), keys)
            self.assertEqual(json.loads((self.fork.directory / "config/rollup.json").read_text()),
                             initial["rollup_config"])
            devnet.main()
            self.assertEqual(inspect.call_count, 3)
            self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest, after)
            devnet.write_json(config_path, {**config, "epoch_slots": 99})
            with self.assertRaisesRegex(RuntimeError, "config changed"):
                devnet.main()
            self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest, after)

    def test_init_refuses_to_mark_prepared_with_a_noncanonical_snapshot_origin(self):
        initial = snapshot()
        initial["rollup_config"]["l1_system_config_address"] = "0x" + "1" * 40
        header = {"number": "0x64", "hash": "0xf", "timestamp": "0x4d8", "parentHash": "0xe"}
        discovered = {"number": 100, "hash": "0xf", "timestamp": 1240, "parentHash": "0xe"}

        def transport(url, method, *args, **kwargs):
            if method == "eth_getBlockByNumber":
                # Every other check passes: only the snapshot's L1 origin 0x13 was reorged upstream.
                return {"number": "0x13", "hash": "0xreorged"} if args[0] == "0x13" else header
            return "0x1" if method == "eth_chainId" else "0x01"

        def contract(url, address, signature, *args, **kwargs):
            return {"getSchedule()(uint64[])": [0] * 14, "minimumProtocolVersion()(uint256)": 1}.get(
                signature, "0x" + "2" * 40)

        with patch.object(devnet, "run", side_effect=lambda *args, **_: "sha256:" + "d" * 64 if args[0] == "docker"
                          else "0x" + args[-1][-40:]), \
                patch.dict(os.environ, {"TEST_EXECUTION_URL": "https://rpc.invalid/key",
                                        "TEST_BEACON_URL": "https://rpc.invalid/key"}), \
                patch.object(devnet, "rpc", side_effect=transport), \
                patch.object(devnet, "call", side_effect=contract), \
                patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                patch.object(devnet.SnapshotFork, "inspect",
                             return_value=[{**copy.deepcopy(initial), "fork": discovered}, copy.deepcopy(initial)]), \
                patch("builtins.print"):
            fork = devnet.SnapshotFork(self.root / "new")
            fork.directory.mkdir()
            with self.assertRaisesRegex(RuntimeError, "noncanonical L1 origin"):
                fork.initialize(self.preparation(execution_env="TEST_EXECUTION_URL", beacon_env="TEST_BEACON_URL"),
                                allow_write=True)
        stored = devnet.SnapshotFork(fork.directory).manifest
        self.assertEqual(stored["phase"], "inspecting")
        self.assertFalse({"fork", "initial", "schedule"} & stored.keys())
        self.assertFalse((fork.directory / "config/rollup.json").exists())

    def test_malformed_remote_numbers_are_reported_without_their_value(self):
        config_path = self.root / "input.json"
        devnet.write_json(config_path, self.preparation())
        for chain_id, slot in (("0xtoken-secret", "12"), ("0x1", "token-secret")):
            with self.subTest(chain_id=chain_id, slot=slot), \
                    patch.object(sys, "argv", ["launcher", "init", "--dir", str(self.root / "new"),
                                               "--config", str(config_path), "--allow-write"]), \
                    patch.object(devnet, "run", side_effect=lambda *args, **_: "sha256:" + "d" * 64
                                 if args[0] == "docker" else "0x" + args[-1][-40:]), \
                    patch.dict(os.environ, {"SNAPSHOT_UPSTREAM_EXECUTION": "https://rpc.invalid/key",
                                            "SNAPSHOT_UPSTREAM_BEACON": "https://rpc.invalid/key"}), \
                    patch.object(devnet, "rpc", return_value=chain_id), \
                    patch.object(devnet, "request_json",
                                 return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": slot}}), \
                    patch.object(devnet.SnapshotFork, "inspect") as inspect, patch("builtins.print"):
                # main reports the exception text verbatim.
                with self.assertRaises(ValueError) as caught:
                    devnet.main()
                self.assertNotIn("token-secret", str(caught.exception))
                self.assertIn("value redacted", str(caught.exception))
                inspect.assert_not_called()
        self.assertEqual([devnet.number(value) for value in (12, "12", "0xc", "0xC")], [12] * 4)

    def test_init_requires_opt_in_unused_directory_and_the_fork_lock_before_writes(self):
        # The lock is main's contract: SnapshotFork.prepare/initialize assume their caller holds it.
        config_path = self.root / "input.json"
        devnet.write_json(config_path, self.preparation())
        target = self.root / "new-fork"

        def init(*extra):
            with patch.object(sys, "argv", ["launcher", "init", "--dir", str(target), "--config", str(config_path),
                                           *extra]), patch.object(devnet, "run") as run:
                devnet.main()
            run.assert_not_called()

        with self.assertRaisesRegex(RuntimeError, "requires --allow-write"):
            init()
        self.assertEqual([entry.name for entry in target.iterdir()], [".lock"])
        with open(target / ".lock") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with self.assertRaises(BlockingIOError):
                init("--allow-write")
        (target / "notes.txt").write_text("unrelated")
        with self.assertRaisesRegex(RuntimeError, "unrecognized data"):
            init("--allow-write")
        self.assertEqual(sorted(entry.name for entry in target.iterdir()), [".lock", "notes.txt"])

    def test_init_resumes_after_its_first_manifest_write_was_interrupted(self):
        config_path, config = self.root / "input.json", self.preparation()
        devnet.write_json(config_path, config)
        target = self.root / "new-fork"
        target.mkdir()
        (target / ".lock").touch()
        self.kill_while_writing(target / "manifest.json")
        self.assertEqual(len(list(target.iterdir())), 2)
        with patch.object(sys, "argv", ["launcher", "init", "--dir", str(target), "--config", str(config_path),
                                       "--allow-write"]), \
                patch.object(devnet.SnapshotFork, "initialize") as initialize:
            devnet.main()
        initialize.assert_called_once_with(config, True)

    def test_boundary_requires_matching_snapshots_with_origins_before_fork(self):
        initial = snapshot()
        devnet.validate_boundary([initial, copy.deepcopy(initial)], 19)
        with self.assertRaisesRegex(RuntimeError, "origin is after the fork"):
            devnet.validate_boundary([initial, initial], 18)
        other = copy.deepcopy(initial)
        other["latest"]["system_config"]["batcherAddr"] = "0xother"
        with self.assertRaisesRegex(RuntimeError, "heads or system configs"):
            devnet.validate_boundary([initial, other], 19)

    def test_snapshot_l1_origins_must_be_canonical_upstream_and_on_restored_local_l1(self):
        initial = snapshot()
        for upstream, pattern in ((True, "noncanonical L1 origin"), (False, "missing/conflicting L1 history")):
            with patch.object(devnet, "rpc", return_value={"hash": "0x19"}) as transport:
                devnet.validate_origins([initial], "https://rpc.invalid", upstream=upstream)
                self.assertEqual(transport.call_args.kwargs, {"upstream": upstream})
            for header in ({"hash": "0xother"}, None):
                with self.subTest(upstream=upstream, header=header), patch.object(devnet, "rpc", return_value=header):
                    with self.assertRaisesRegex(RuntimeError, pattern):
                        devnet.validate_origins([initial], "https://rpc.invalid", upstream=upstream)

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
        with self.assertRaisesRegex(RuntimeError, "not finalized"):
            devnet.validate_fork(fork, header, None, 1000, 12)
        off_grid = {**fork, "timestamp": 1241}
        with self.assertRaisesRegex(RuntimeError, "slot grid"):
            devnet.validate_fork(off_grid, {**header, "timestamp": hex(1241)}, {"number": "0x65"}, 1000, 12)

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

    def test_slot_grid_skips_downtime_without_reusing_future_tip(self):
        self.assertEqual(devnet.next_slot(1000, 12, 1240, 1293), 1300)
        self.assertEqual(devnet.next_slot(1000, 12, 1312, 1293), 1324)
        self.assertEqual(devnet.next_slot(1000, 12, 1240, 1252), 1252)
        with self.assertRaises(RuntimeError):
            devnet.next_slot(1000, 0, 1240, 1252)

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

    def local_l1(self, apply_writes=True, fail_send=False, status="0x1"):
        """Patches a local Anvil fork whose sent SystemConfig writes take effect when a block is mined.

        Returns the ordered RPC method log and the persisted journal observed at each send and mine.
        """
        self.fork.manifest["system_config"] = "0x" + "5" * 40
        system = {"owner": "0x" + "a" * 40, "batcherHash": "0x0", "unsafeBlockSigner": "0x0"}
        pending, receipts, calls, journals = [], {}, [], []

        def node(url, method, *params):
            self.assertEqual(url, "http://127.0.0.1:19545")
            calls.append(method)
            if method == "anvil_metadata":
                return {"chainId": "0x1", "forkedNetwork": {"forkBlockHash": "0xf", "forkBlockNumber": "0x64"}}
            if method in ("eth_sendTransaction", "evm_mine"):
                journals.append(json.loads((self.fork.directory / "manifest.json").read_text())["operations"])
            if method == "eth_sendTransaction":
                if fail_send:
                    raise devnet.Unavailable("connection reset after the request was written")
                pending.append(params[0])
                return f"0xtx{len(receipts) + len(pending)}"
            if method == "evm_mine":
                for transaction in pending:
                    signature, argument = transaction["data"].split("|")
                    if apply_writes:
                        system[{"setBatcherHash(bytes32)": "batcherHash",
                                "setUnsafeBlockSigner(address)": "unsafeBlockSigner"}[signature]] = argument
                    receipts[f"0xtx{len(receipts) + 1}"] = {"status": status}
                pending.clear()
            if method == "eth_getTransactionReceipt":
                return receipts.get(params[0])
            return {"eth_getTransactionCount": "0x7", "eth_estimateGas": "0x5208",
                    "eth_getBlockByNumber": {"timestamp": hex(1240)}}.get(method)

        self.addCleanup(patch.stopall)
        patch.object(devnet, "rpc", side_effect=node).start()
        patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000"}}).start()
        patch.object(devnet, "run", side_effect=lambda *args: "|".join(args[2:])).start()
        patch.object(devnet, "call", side_effect=lambda url, address, signature: system[signature.split("(")[0]]).start()
        patch.object(devnet.time, "time", return_value=1293).start()
        patch.object(devnet.time, "sleep").start()
        patch("builtins.print").start()
        return calls, journals

    def test_local_l1_writes_require_matching_fork_and_beacon_identity(self):
        for name, change in (("fork hash", {"fork": {**manifest()["fork"], "hash": "0xother"}}),
                             ("fork number", {"fork": {**manifest()["fork"], "number": "0x65"}}),
                             ("beacon genesis", {"beacon_genesis": {"genesis_time": "1001"}})):
            with self.subTest(name):
                self.fork.manifest = {**manifest(), **change}
                calls, _ = self.local_l1()
                with self.assertRaisesRegex(RuntimeError, "identity does not match manifest"):
                    self.fork.send("set-signer", "0x1", "0x2", "set(uint256)", 1)
                self.assertEqual(calls, ["anvil_metadata"])
                self.assertFalse((self.fork.directory / "manifest.json").exists())
                patch.stopall()

    def test_send_journals_nonce_before_sending_and_hash_after(self):
        calls, journals = self.local_l1()
        receipt = self.fork.send("set-signer", "0x1", "0x2", "setUnsafeBlockSigner(address)", "0x3")
        self.assertEqual(journals[0]["set-signer"]["transaction"]["nonce"], "0x7")
        self.assertNotIn("hash", journals[0]["set-signer"])
        self.assertEqual(journals[1]["set-signer"]["hash"], "0xtx1", "persist the hash before awaiting a receipt")
        stored = devnet.SnapshotFork(self.fork.directory).manifest["operations"]["set-signer"]
        self.assertEqual((stored["hash"], stored["receipt"]), ("0xtx1", receipt))
        self.assertLess(calls.index("eth_sendTransaction"), calls.index("anvil_stopImpersonatingAccount"))
        self.assertLess(calls.index("anvil_stopImpersonatingAccount"), calls.index("evm_mine"))

    def test_reverted_send_persists_its_receipt_and_is_never_resent(self):
        calls, _ = self.local_l1(status="0x0")
        for _ in range(2):
            with self.assertRaisesRegex(RuntimeError, "set-signer reverted; inspect its persisted receipt"):
                self.fork.send("set-signer", "0x1", "0x2", "setUnsafeBlockSigner(address)", "0x3")
            self.fork = devnet.SnapshotFork(self.fork.directory, timeout=1)
            self.assertEqual(self.fork.manifest["operations"]["set-signer"]["receipt"], {"status": "0x0"})
        self.assertEqual(calls.count("eth_sendTransaction"), 1)
        # A resumed send re-reads its receipt: restored L1 may differ from the journal.
        self.assertEqual(calls[-1], "eth_getTransactionReceipt")

    def test_send_without_a_recorded_hash_is_never_resent(self):
        calls, _ = self.local_l1(fail_send=True)
        with self.assertRaises(devnet.Unavailable):
            self.fork.send("set-signer", "0x1", "0x2", "setUnsafeBlockSigner(address)", "0x3")
        self.assertEqual(calls[-1], "anvil_stopImpersonatingAccount")
        stored = devnet.SnapshotFork(self.fork.directory).manifest["operations"]["set-signer"]
        self.assertEqual((stored["transaction"]["nonce"], "hash" in stored), ("0x7", False))
        calls.clear()
        with self.assertRaisesRegex(RuntimeError, "interrupted; reconcile its nonce"):
            self.fork.send("set-signer", "0x1", "0x2", "setUnsafeBlockSigner(address)", "0x3")
        self.assertEqual(calls, ["anvil_metadata"])

    def test_bootstrap_authorizes_batcher_and_signer_once_through_journaled_owner_sends(self):
        calls, _ = self.local_l1()
        accounts = self.fork.manifest["accounts"]
        self.fork.bootstrap()
        writes = [method for method in calls if method in ("eth_sendTransaction", "evm_mine", "anvil_setBalance")]
        self.assertEqual(writes, ["anvil_setBalance", "eth_sendTransaction", "evm_mine"] * 2 + ["anvil_setBalance"])
        operations = devnet.SnapshotFork(self.fork.directory).manifest["operations"]
        self.assertEqual(operations["set-batcher"]["transaction"]["from"], "0x" + "a" * 40)
        self.assertEqual(operations["set-batcher"]["transaction"]["data"],
                         "setBatcherHash(bytes32)|0x" + accounts["batcher"][2:].zfill(64))
        self.assertEqual(operations["set-signer"]["transaction"]["data"],
                         "setUnsafeBlockSigner(address)|" + accounts["signer"])
        calls.clear()
        self.fork.bootstrap()
        self.assertNotIn("eth_sendTransaction", calls)
        self.fork.manifest["operations"] = {}
        self.local_l1(apply_writes=False)
        with self.assertRaisesRegex(RuntimeError, "batcher update not applied"):
            self.fork.bootstrap()

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

    def test_upgrade_gate_requires_both_nodes_ready_with_the_recorded_schedule(self):
        self.fork.manifest["schedule"] = [100] * 12 + [800]
        recorded = {upgrade + "_time": 100 for upgrade in devnet.UPGRADES[:devnet.LEGACY_UPGRADE_COUNT]}
        recorded["base"] = {"azul": 100, "beryl": 100, "cobalt": 800}
        stale = {**recorded, "base": {**recorded["base"], "cobalt": None}}
        self.fork.timeout = 0.02
        for config, ready, observed in ((recorded, True, True), (stale, True, False), (recorded, False, False)):
            with self.subTest(config=config["base"], ready=ready), \
                    patch.object(self.fork, "url", side_effect=lambda role: role), \
                    patch.object(devnet, "rpc", side_effect=lambda url, method: (
                        {"ready": url == "sequencer-cl" or ready} if method == "base_upgradeReadiness"
                        else recorded if url == "sequencer-cl" else config)), \
                    patch.object(devnet.time, "sleep"), patch("builtins.print"):
                if observed:
                    self.fork.wait_upgrades()
                else:
                    with self.assertRaisesRegex(RuntimeError, "timed out: validator observing"):
                        self.fork.wait_upgrades()

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
            # A node returns only blocks at or below its derived safe head.
            return {"hash": hashes[url]} if current[url] >= devnet.number(args[0]) else None
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

    def test_checkpoint_capture_saves_reachable_nodes_before_reporting_identity_failures(self):
        validator = sync_status(101, safe=129, safe_hash="0x129")
        for failure in (devnet.Unavailable("sequencer stopped"), RuntimeError("ambiguous running containers")):
            with self.subTest(failure=failure):
                self.fork.manifest["last_stop"] = {"sequencer": {"safe_l2": {"number": 1, "hash": "0xold"}}}
                self.fork.save()
                def status(role):
                    if role == "sequencer":
                        raise failure
                    return validator
                with patch.object(self.fork, "sync_status", side_effect=status):
                    if isinstance(failure, devnet.Unavailable):
                        self.fork.record_checkpoints()
                    else:
                        with self.assertRaisesRegex(RuntimeError, "ambiguous"):
                            self.fork.record_checkpoints()
                # The unreadable sequencer keeps its earlier checkpoint for the next start to restore.
                self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["last_stop"], {
                    "sequencer": {"safe_l2": {"number": 1, "hash": "0xold"}}, "validator": validator})

    def test_unrestored_or_unreachable_checkpoints_survive_repeated_stops_until_replayed(self):
        recorded = {"safe_l2": {"number": 129, "hash": "0x129"}, "finalized_l2": {"number": 125, "hash": "0x125"}}
        saved = {role: copy.deepcopy(recorded) for role in devnet.ROLES}
        self.fork.manifest["last_stop"] = copy.deepcopy(saved)
        self.fork.save()
        # The validator's replay was interrupted at the persisted head 123; the sequencer is unreachable.
        live = {"validator": sync_status(101, safe=123)}

        def status(role):
            if role not in live:
                raise devnet.Unavailable(role + " consensus RPC stopped")
            return live[role]

        def node(url, method, height, full):
            self.assertEqual(url, "validator")
            return {"hash": "0x" + str(devnet.number(height))}

        with patch.object(devnet.SnapshotFork, "sync_status", side_effect=status), \
                patch.object(devnet.SnapshotFork, "url", side_effect=lambda role: role), \
                patch.object(devnet, "rpc", side_effect=node), \
                patch.object(devnet.time, "sleep"), patch("builtins.print"):
            for _ in range(2):  # Stop mid-replay, restart, and stop mid-replay again.
                self.fork.record_checkpoints()
                restarted = devnet.SnapshotFork(self.fork.directory, timeout=0.02)
                self.assertEqual(restarted.manifest["last_stop"], saved)
                with self.assertRaisesRegex(RuntimeError, "no progress"):
                    restarted.wait_checkpoints()
            # Once the exact recorded hashes are restored, only that role's status is replaced.
            live["validator"] = sync_status(101, safe=130, safe_hash="0x130")
            self.fork.record_checkpoints()
        self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["last_stop"],
                         {"sequencer": saved["sequencer"], "validator": live["validator"]})

    def test_uninitialized_engine_status_never_becomes_a_checkpoint(self):
        recorded = {"safe_l2": {"number": 129, "hash": "0x129"}, "finalized_l2": {"number": 125, "hash": "0x125"}}
        self.fork.manifest["last_stop"] = {"validator": recorded}
        # A consensus node reports zeroed heads until its engine state is initialized from the EL.
        zero = sync_status(0, safe=0, safe_hash="0x" + "0" * 64)
        with patch.object(self.fork, "sync_status", return_value=zero), \
                patch.object(self.fork, "url", side_effect=lambda role: role), \
                patch.object(devnet, "rpc", return_value={"hash": "0x" + "0" * 64}):
            self.fork.record_checkpoints()
        self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["last_stop"], {"validator": recorded})

    def test_higher_conflicting_safe_head_cannot_replace_recorded_checkpoints(self):
        recorded = {"safe_l2": {"number": 129, "hash": "0x129"}, "finalized_l2": {"number": 125, "hash": "0x125"}}
        self.fork.manifest["last_stop"] = {"validator": recorded}
        statuses = {"sequencer": sync_status(101, safe=140, safe_hash="0x140"),
                    "validator": sync_status(101, safe=140, safe_hash="0xother140")}

        def node(url, method, height, full):
            self.assertEqual(url, "validator")
            return {"hash": "0xother" + str(devnet.number(height))}

        with patch.object(self.fork, "sync_status", side_effect=statuses.get), \
                patch.object(self.fork, "url", side_effect=lambda role: role), \
                patch.object(devnet, "rpc", side_effect=node):
            with self.assertRaisesRegex(RuntimeError, "validator derived a block conflicting with a recorded checkpoint"):
                self.fork.record_checkpoints()
        self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["last_stop"],
                         {"sequencer": statuses["sequencer"], "validator": recorded})

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
                        patch.object(devnet, "rpc", return_value=actual), patch("builtins.print"):
                    if deferred:
                        self.assertEqual(len(self.fork.inspect()), 2)
                    else:
                        with self.assertRaisesRegex(RuntimeError, "checkpoint"):
                            self.fork.inspect()

    def test_checkpoint_replay_requires_exact_saved_hashes_after_safe_derivation_using_reads_only(self):
        for outcome in ("recovered", "unavailable", "conflict", "stalled"):
            with self.subTest(outcome=outcome):
                # Block 129 is above the persisted head 123, so inspection deferred it to this gate.
                self.fork.manifest["last_stop"] = {
                    "validator": {"safe_l2": {"number": 129, "hash": "0xsafe"},
                                  "finalized_l2": {"number": 125, "hash": "0xfinal"}}}
                self.fork.timeout = 0.02
                polls, methods = [], []

                def status(role):
                    self.assertEqual(role, "validator")
                    polls.append(role)
                    if outcome == "unavailable" and len(polls) == 1:
                        raise devnet.Unavailable("validator consensus RPC restarting")
                    return sync_status(101, safe=129 if len(polls) > 1 and outcome != "stalled" else 123, unsafe=130)

                def node(url, method, height, full):
                    methods.append(method)
                    self.assertGreaterEqual(len(polls), 2, "compare hashes only once safe derivation reaches them")
                    return {"hash": "0xwrong" if outcome == "conflict" else
                            {129: "0xsafe", 125: "0xfinal"}[devnet.number(height)]}

                with patch.object(self.fork, "sync_status", side_effect=status), \
                        patch.object(self.fork, "url", side_effect=lambda role: role), \
                        patch.object(devnet, "rpc", side_effect=node), \
                        patch.object(devnet.time, "sleep"), patch("builtins.print"):
                    if outcome in ("recovered", "unavailable"):
                        self.fork.wait_checkpoints()
                        self.assertEqual(methods, ["eth_getBlockByNumber"] * 2)
                    else:
                        with self.assertRaisesRegex(RuntimeError, "conflicting with a recorded checkpoint"
                                                    if outcome == "conflict" else "no progress"):
                            self.fork.wait_checkpoints()
                        self.assertEqual(set(methods), {"eth_getBlockByNumber"} if outcome == "conflict" else set())

    def test_up_and_down_dispatch_to_nondestructive_lifecycle_for_an_explicit_initialized_fork(self):
        self.fork.save()
        for command, method in (("up", "start"), ("start", "start"), ("down", "stop"), ("stop", "stop")):
            with self.subTest(command=command), \
                    patch.object(sys, "argv", ["snapshot_devnet.py", command, "--dir", str(self.fork.directory)]), \
                    patch.object(devnet.SnapshotFork, method) as lifecycle:
                devnet.main()
                lifecycle.assert_called_once_with()
        for argv, error in ((["up"], RuntimeError), (["up", "--dir", str(self.root / "missing")], RuntimeError),
                            (["status", "--dir", str(self.root)], RuntimeError)):
            with self.subTest(argv=argv), patch.object(sys, "argv", ["launcher", *argv]), \
                    patch.object(devnet, "run") as run, patch("sys.stderr"):
                with self.assertRaises(error):
                    devnet.main()
                run.assert_not_called()

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
                self.assertEqual(fork.timeout, 7200)
                self.assertEqual(fork.endpoint("execution"), "https://rpc.invalid/secret-key")
        devnet.write_json(self.fork.directory / "keys.json", {"signer": "0x1", "batcher": "0x2"})
        fork = devnet.SnapshotFork(self.fork.directory)
        self.assertEqual(fork.compose_env()["SNAPSHOT_BEACON"], "https://rpc.invalid/secret-key")

    def test_setup_dir_resumes_only_recorded_initialization(self):
        with patch.object(sys, "argv", ["launcher", "setup", "--dir", str(self.fork.directory)]), \
                patch.object(devnet, "rpc") as rpc:
            with self.assertRaisesRegex(RuntimeError, "requires an initialized fork; use --workdir"):
                devnet.main()
            rpc.assert_not_called()
        self.assertFalse(devnet.setup_path().exists())
        self.assertFalse((self.fork.directory / "upstreams.json").exists())
        self.fork.manifest["phase"] = "inspecting"
        for setup_input in (None, {"sequencer_datadir": "/builder"}):
            with self.subTest(setup_input=setup_input), \
                    patch.object(sys, "argv", ["launcher", "setup", "--dir", str(self.fork.directory)]), \
                    patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                    patch.object(devnet, "rpc", return_value="0x1"), \
                    patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                    patch.object(devnet, "prepare_snapshot") as prepare, \
                    patch.object(devnet.SnapshotFork, "initialize") as initialize, patch("builtins.print"):
                if setup_input:
                    self.fork.manifest["setup_input"] = setup_input
                self.fork.save()
                if setup_input:
                    devnet.main()
                    initialize.assert_called_once_with(setup_input, allow_write=True)
                    self.assertEqual(json.loads(devnet.setup_path().read_text()), {"directory": str(self.fork.directory)})
                else:
                    with self.assertRaisesRegex(RuntimeError, "use init with the original input config"):
                        devnet.main()
                    initialize.assert_not_called()
                prepare.assert_not_called()

    def test_setup_rejects_ambiguous_or_unsupported_selection_and_rereads_manifest_under_lock(self):
        for options, change, error in ((["--workdir", str(self.root / "new")], {}, "choose either"),
                                       ([], {"version": 1}, "unsupported snapshot manifest"),
                                       ([], {"phase": "retired"}, "unsupported snapshot manifest")):
            with self.subTest(change=change, error=error), patch.object(devnet, "rpc") as rpc, \
                    patch.object(sys, "argv", ["launcher", "setup", "--dir", str(self.fork.directory), *options]):
                devnet.write_json(self.fork.directory / "manifest.json", {**manifest(), **change})
                with self.assertRaisesRegex(RuntimeError, error):
                    devnet.main()
                rpc.assert_not_called()
        self.assertEqual(sorted(path.name for path in self.root.iterdir()), ["fork"])
        work = self.root / "work"
        flock = fcntl.flock

        def concurrent_init(lock, operation):
            # Another setup finished initialization after this one first read the fork.
            flock(lock, operation)
            devnet.write_json(work / "fork/manifest.json", manifest())

        with patch.object(sys, "argv", ["launcher", "setup", "--workdir", str(work)]), \
                patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                patch.object(devnet, "rpc", return_value="0x1"), \
                patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                patch.object(devnet.fcntl, "flock", side_effect=concurrent_init), \
                patch.object(devnet, "prepare_snapshot") as prepare, patch("builtins.print"):
            devnet.main()
            prepare.assert_not_called()

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

    def test_setup_beacon_requests_extend_the_endpoint_path_before_its_query(self):
        self.fork.save()
        # Query credentials may end in "/"; requests and saved endpoints keep them intact.
        execution, beacon = "https://rpc.invalid/v2?key=secret-rpc/", "https://beacon.invalid/path?token=secret-beacon/"
        requests = []

        def respond(request, timeout):
            requests.append(request)
            if opener.failure and not request.data:
                raise opener.failure
            return io.BytesIO(b'{"result": "0x1"}' if request.data else
                              b'{"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}')

        opener = Mock(failure=None)
        opener.open.side_effect = respond
        argv = ["launcher", "setup", "--dir", str(self.fork.directory)]
        for name, environment, expected, saved in (
                ("configured Beacon", {"TEST_EXECUTION_URL": execution, "TEST_BEACON_URL": beacon},
                 ["https://beacon.invalid/path/eth/v1/beacon/genesis?token=secret-beacon/",
                  "https://beacon.invalid/path/eth/v1/config/spec?token=secret-beacon/"], beacon),
                ("execution fallback", {"TEST_EXECUTION_URL": execution},
                 ["https://rpc.invalid/v2/eth/v1/beacon/genesis?key=secret-rpc/",
                  "https://rpc.invalid/v2/eth/v1/config/spec?key=secret-rpc/"], execution)):
            with self.subTest(name=name), patch.object(sys, "argv", argv), \
                    patch.dict(os.environ, environment), \
                    patch.object(devnet.urllib.request, "build_opener", return_value=opener), \
                    patch.object(devnet.getpass, "getpass", side_effect=AssertionError("configured endpoint must not prompt")), \
                    patch("builtins.print") as output:
                (self.fork.directory / "upstreams.json").unlink(missing_ok=True)
                requests.clear()
                devnet.main()
                self.assertEqual([request.full_url for request in requests], [execution, *expected])
                self.assertNotIn("secret", devnet.setup_path().read_text() + str(output.call_args_list))
                self.assertEqual(json.loads((self.fork.directory / "upstreams.json").read_text()),
                                 {"execution": execution, "beacon": saved})
        opener.failure = devnet.urllib.error.URLError(beacon)
        with patch.object(sys, "argv", argv), \
                patch.dict(os.environ, {"TEST_EXECUTION_URL": execution, "TEST_BEACON_URL": beacon}), \
                patch.object(devnet.urllib.request, "build_opener", return_value=opener):
            with self.assertRaisesRegex(RuntimeError, "configured Beacon endpoint failed validation") as caught:
                devnet.main()
        self.assertNotIn("secret", str(caught.exception))

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
        self.assertFalse(devnet.setup_path().exists())

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

    @unittest.skipUnless(shutil.which("rsync"), "requires rsync, but copies only temporary data")
    def test_setup_prompts_for_new_workdir_and_records_pending_work_before_preparing(self):
        home = self.root / "home with spaces"
        for failed in (None, "download", "copy"):
            with self.subTest(failed=failed):
                work = self.root / f"{failed} experiment" if failed else home / "data/snapshot-devnet"
                commands = []
                image = "sha256:" + "b" * 64

                def command(*args, lock_fd):
                    commands.append(args)
                    if args[:2] == ("docker", "run"):
                        self.assertEqual(args[args.index("--download-concurrency") + 1], "32" if failed else "16")
                        (work / "builder/db").mkdir(parents=True)
                        (work / "builder/db/mdbx.dat").write_bytes(b"snapshot-state")
                        if failed == "download":
                            raise RuntimeError("download interrupted")
                    elif args[0] == "rsync":
                        if failed == "copy":
                            (work / "validator/db").mkdir(parents=True)
                            (work / "validator/db/mdbx.dat").write_bytes(b"partial")
                            raise RuntimeError("copy interrupted")
                        subprocess.run(args, check=True, capture_output=True)

                def initialize(fork, config, allow_write):
                    fork.manifest = manifest()
                    fork.manifest["upstreams"] = {"execution": "SNAPSHOT_UPSTREAM_EXECUTION",
                                                  "beacon": "SNAPSHOT_UPSTREAM_BEACON"}
                    self.assertEqual(fork.endpoint("execution"), "https://rpc.invalid/new-key")
                    self.assertEqual(fork.endpoint("beacon"), "https://beacon.invalid/key")
                    self.assertEqual({config[role + "_image"] for role in ("base", "anvil", "batcher")}, {image})
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
                        self.assertEqual(json.loads(devnet.setup_path().read_text()),
                                         {"directory": saved, "pending_directory": str(work / "fork")})
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
                    self.assertEqual(json.loads((work / "fork/upstreams.json").read_text()),
                                     {"execution": "https://rpc.invalid/new-key", "beacon": "https://beacon.invalid/key"})
                    persisted = (work / "input.json").read_text() + (work / "setup.json").read_text() + devnet.setup_path().read_text()
                    self.assertNotIn(".invalid", persisted)

    @unittest.skipUnless(shutil.which("rsync"), "requires rsync, but copies only temporary data")
    def test_setup_resumes_saved_workdir_and_completed_setup_does_no_preparation(self):
        phases = ("build", "inspector", "download", "copy", "initialize")
        for failure in phases[1:]:
            with self.subTest(failure=failure):
                work = self.root / failure
                calls, fetched = [], []

                def step(phase):
                    calls.append(phase)
                    if phase == failure and calls.count(phase) == 1:
                        raise KeyboardInterrupt()

                def command(*args, lock_fd):
                    step({"cargo": "inspector", "rsync": "copy"}.get(args[0]) or {"buildx": "build", "run": "download"}[args[1]])
                    if args[:2] == ("docker", "run"):
                        (work / "builder/db").mkdir(parents=True)
                        (work / "builder/db/mdbx.dat").write_bytes(b"snapshot")
                    elif args[0] == "rsync":
                        subprocess.run(args, check=True, capture_output=True)

                def initialize(fork, config, allow_write):
                    fork.manifest = {**manifest(), "phase": "inspecting", "setup_input": config}
                    # Initialization reads the endpoints setup saved, not exported variables.
                    self.assertEqual(fork.endpoint("execution"), "https://rpc.invalid/key")
                    fork.save()
                    step("initialize")
                    fork.manifest["phase"] = "prepared"
                    fork.save()

                environment = dict(os.environ)
                with patch.object(sys, "argv", ["launcher", "setup", "--workdir", str(work)]), \
                        patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key") as prompt, \
                        patch.object(devnet, "rpc", return_value="0x1"), \
                        patch("builtins.input", side_effect=AssertionError("resume must remember its workdir")), \
                        self.preparing(command, initialize, fetched):
                    with self.assertRaises(KeyboardInterrupt):
                        devnet.main()
                    self.assertEqual(json.loads(devnet.setup_path().read_text())["pending_directory"], str(work / "fork"))
                    with patch.object(sys, "argv", ["launcher", "setup"]):
                        devnet.main()
                        before = list(calls)
                        devnet.main()
                        self.assertEqual(calls, before, "completed setup must do no preparation")
                    prompt.assert_called_once()
                self.assertEqual(calls, [phase for phase in phases for _ in range(2 if phase == failure else 1)])
                self.assertEqual(fetched.count(devnet.SNAPSHOT_INDEX), 1)
                self.assertEqual(json.loads(devnet.setup_path().read_text()), {"directory": str(work / "fork")})
                self.assertEqual(dict(os.environ), environment, "setup must not export credentials")

    def test_setup_resumes_after_its_first_journal_write_was_interrupted(self):
        work, commands, write_json = self.root / "work", [], devnet.write_json

        def journal(path, value):
            if path.name == "setup.json":
                self.kill_while_writing(path, write_json)
                raise RuntimeError("killed")  # The launcher died during its first journal write.
            write_json(path, value)

        def command(*args, lock_fd):
            commands.append(args)
            raise RuntimeError("build interrupted")

        def setup(*options):
            with patch.object(sys, "argv", ["launcher", "setup", *options]), \
                    patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                    patch.object(devnet, "rpc", return_value="0x1"), self.preparing(command, None, []):
                devnet.main()

        with patch.object(devnet, "write_json", side_effect=journal), self.assertRaisesRegex(RuntimeError, "killed"):
            setup("--workdir", str(work))
        leftovers = sorted(entry.name for entry in work.iterdir())
        self.assertEqual((len(leftovers), leftovers[0]), (2, "fork"))
        with self.assertRaisesRegex(RuntimeError, "build interrupted"):
            setup()
        self.assertEqual(json.loads((work / "setup.json").read_text())["phase"], "build")
        self.assertEqual(commands[0][:3], ("docker", "buildx", "bake"))
        # Unrecognized files in a remembered workdir are still never adopted.
        (work / "setup.json").unlink()
        (work / "notes.txt").write_text("unrelated")
        with self.assertRaisesRegex(RuntimeError, "unused working directory"):
            setup()
        self.assertEqual(len(commands), 1)
        self.assertEqual((work / "notes.txt").read_text(), "unrelated")

    def test_setup_resumes_after_its_first_credentials_write_was_interrupted(self):
        commands, write_json = [], devnet.write_json

        def credentials(path, value):
            if path.name == "upstreams.json":
                self.kill_while_writing(path, write_json)
                raise RuntimeError("killed")  # The very first launcher died while saving the endpoints.
            write_json(path, value)

        def command(*args, lock_fd):
            commands.append(args)
            raise RuntimeError("build interrupted")

        def setup(*options):
            with patch.object(sys, "argv", ["launcher", "setup", *options]), \
                    patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                    patch.object(devnet, "rpc", return_value="0x1"), \
                    patch("builtins.input", side_effect=AssertionError("retry must remember its workdir")), \
                    self.preparing(command, None, []):
                devnet.main()

        for retry in ([], ["--workdir"]):
            with self.subTest(retry=retry):
                work = self.root / ("explicit" if retry else "remembered")
                devnet.setup_path().unlink(missing_ok=True)
                with patch.object(devnet, "write_json", side_effect=credentials), \
                        self.assertRaisesRegex(RuntimeError, "killed"):
                    setup("--workdir", str(work))
                leftovers = sorted(entry.name for entry in (work / "fork").iterdir())
                self.assertEqual((len(leftovers), leftovers[0]), (2, ".lock"))
                with self.assertRaisesRegex(RuntimeError, "build interrupted"):
                    setup(*retry, *([str(work)] if retry else []))
                self.assertEqual(json.loads(devnet.setup_path().read_text()), {"pending_directory": str(work / "fork")})
                self.assertEqual(json.loads((work / "fork/upstreams.json").read_text())["execution"],
                                 "https://rpc.invalid/key")
                self.assertEqual(sorted(entry.name for entry in (work / "fork").iterdir()),
                                 sorted([*leftovers, "upstreams.json"]))
        self.assertEqual(len(commands), 2)

        # Killed after creating the fork's lock, before recording the pending setup.
        work = self.root / "pristine"
        (work / "fork").mkdir(parents=True)
        (work / "fork/.lock").touch()
        devnet.setup_path().unlink()
        with self.assertRaisesRegex(RuntimeError, "build interrupted"):
            setup("--workdir", str(work))
        self.assertEqual(json.loads(devnet.setup_path().read_text()), {"pending_directory": str(work / "fork")})
        self.assertEqual(len(commands), 3)

        # Unrecognized files beside or inside an unrecorded fork directory are never adopted.
        for unknown in ("notes.txt", "fork/notes.txt"):
            with self.subTest(unknown=unknown):
                work = self.root / ("unknown-" + unknown.replace("/", "-"))
                (work / "fork").mkdir(parents=True)
                (work / "fork/.lock").touch()
                (work / unknown).write_text("unrelated")
                devnet.setup_path().unlink(missing_ok=True)
                with self.assertRaisesRegex(RuntimeError, "unused working directory"):
                    setup("--workdir", str(work))
                self.assertEqual((work / unknown).read_text(), "unrelated")
                self.assertEqual(sorted(path.relative_to(work).as_posix() for path in work.rglob("*")),
                                 sorted({"fork", "fork/.lock", unknown}))
                self.assertFalse(devnet.setup_path().exists())
        self.assertEqual(len(commands), 3)

    def test_init_adopts_a_fork_whose_setup_credentials_write_was_interrupted(self):
        work, write_json = self.root / "work", devnet.write_json
        fork = work / "fork"

        def credentials(path, value):
            if path.name == "upstreams.json":
                self.kill_while_writing(path, write_json)
                raise RuntimeError("killed")  # The launcher died while saving the endpoints.
            write_json(path, value)

        def command(*args, lock_fd):
            raise RuntimeError("build interrupted")

        def setup():
            with patch.object(sys, "argv", ["launcher", "setup", "--workdir", str(work)]), \
                    patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                    patch.object(devnet, "rpc", return_value="0x1"), self.preparing(command, None, []):
                devnet.main()

        with self.assertRaisesRegex(RuntimeError, "build interrupted"):
            setup()
        (fork / "upstreams.json").unlink()
        with patch.object(devnet, "write_json", side_effect=credentials), self.assertRaisesRegex(RuntimeError, "killed"):
            setup()
        with self.assertRaisesRegex(RuntimeError, "build interrupted"):
            setup()
        self.assertEqual(json.loads((fork / "upstreams.json").read_text())["execution"], "https://rpc.invalid/key")
        # Older launchers wrote every file through one fixed temporary name.
        (fork / "upstreams.json.tmp").write_text("{}")
        leftovers = sorted(entry.name for entry in fork.iterdir())
        self.assertEqual(len(leftovers), 4)
        config_path, config = self.root / "input.json", self.preparation()
        devnet.write_json(config_path, config)

        def init():
            with patch.object(sys, "argv", ["launcher", "init", "--dir", str(fork), "--config", str(config_path),
                                           "--allow-write"]), \
                    patch.object(devnet.SnapshotFork, "initialize") as initialize:
                devnet.main()
            return initialize

        init().assert_called_once_with(config, True)
        # Only recognized interrupted writes are adopted; other temporary files are never removed.
        (fork / "notes.tmp").write_text("unrelated")
        with self.assertRaisesRegex(RuntimeError, "unrecognized data"):
            init()
        self.assertEqual(sorted(entry.name for entry in fork.iterdir()), sorted([*leftovers, "notes.tmp"]))

    def test_setup_resumes_legacy_initialization_without_recopying(self):
        image = "sha256:" + "b" * 64
        devnet.setup_path().parent.mkdir(parents=True)
        for name in ("changed", "legacy"):
            with self.subTest(name=name):
                work = self.root / name
                builder, validator = self.datadir(f"{name}/builder"), self.datadir(f"{name}/validator")
                config = {"sequencer_datadir": str(builder), "validator_datadir": str(validator),
                          "port": 19545, **{role + "_image": image for role in ("base", "anvil", "batcher")}}
                fork = devnet.SnapshotFork(work / "fork")
                fork.directory.mkdir()
                fork.manifest = {**manifest(), "phase": "inspecting", "setup_input": config}
                fork.save()
                # Older setups saved this input and remembered the working directory, but kept no journal.
                devnet.write_json(work / "input.json",
                                  {**config, "validator_datadir": str(builder)} if name == "changed" else config)
                devnet.write_json(devnet.setup_path(), {"directory": str(work)})

                def initialize(fork, saved, allow_write):
                    self.assertEqual(saved, config)
                    fork.manifest["phase"] = "prepared"
                    fork.save()

                with patch.object(sys, "argv", ["launcher", "setup"]), \
                        patch("builtins.input", side_effect=AssertionError("initialized data must not be recopied")), \
                        patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                        patch.object(devnet, "rpc", return_value="0x1"), \
                        patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                        patch.object(devnet.shutil, "which", return_value="/usr/bin/tool"), \
                        patch.object(devnet, "setup_command", side_effect=AssertionError("must not build, download or copy")), \
                        patch.object(devnet.SnapshotFork, "initialize", autospec=True, side_effect=initialize) as init, \
                        patch("builtins.print"):
                    if name == "changed":
                        with self.assertRaisesRegex(RuntimeError, "input changed"):
                            devnet.main()
                        init.assert_not_called()
                        self.assertFalse((work / "setup.json").exists())
                        continue
                    devnet.main()
                    init.assert_called_once()
                self.assertEqual(json.loads((work / "setup.json").read_text())["phase"], "initialize")
                self.assertEqual(devnet.configured_directory(), work / "fork")
                self.assertEqual((validator / "db/mdbx.dat").read_bytes(), b"untouched")

    def test_setup_resume_rejects_image_options_that_differ_from_its_journal(self):
        work, commands = self.root / "work", []

        def command(*args, lock_fd):
            commands.append(args)
            raise RuntimeError("build interrupted")  # Each run stops at its first build.

        def setup(*options, directory=("--workdir", str(work))):
            with patch.object(sys, "argv", ["launcher", "setup", *directory, *options]), \
                    patch.object(devnet.getpass, "getpass", return_value="https://rpc.invalid/key"), \
                    patch.object(devnet, "rpc", return_value="0x1"), self.preparing(command, None, []):
                devnet.main()

        with self.assertRaisesRegex(RuntimeError, "build interrupted"):
            setup("--anvil-image", "anvil:custom")
        journal = json.loads((work / "setup.json").read_text())
        self.assertEqual(journal["images"], {**devnet.DEFAULT_IMAGES, "anvil": "anvil:custom"})
        for options in (["--anvil-image", "anvil:other"], ["--batcher-image", "op-batcher:other"],
                        ["--anvil-image", devnet.DEFAULT_IMAGES["anvil"]]):
            with self.subTest(options=options), self.assertRaisesRegex(RuntimeError, "image options"):
                setup(*options)
        self.assertEqual(len(commands), 1)
        for options in ([], ["--anvil-image", "anvil:custom"], ["--batcher-image", devnet.DEFAULT_IMAGES["batcher"]]):
            with self.subTest(options=options), self.assertRaisesRegex(RuntimeError, "build interrupted"):
                setup(*options)
        self.assertEqual(len(commands), 4)
        self.assertEqual(json.loads((work / "setup.json").read_text()), journal)
        # Journals written before tags were recorded resume without options but cannot verify any.
        devnet.write_json(work / "setup.json", {key: value for key, value in journal.items() if key != "requested_images"})
        with self.assertRaisesRegex(RuntimeError, "build interrupted"):
            setup()
        with self.assertRaisesRegex(RuntimeError, "image options"):
            setup("--anvil-image", "anvil:custom")
        self.fork.save()
        with self.assertRaisesRegex(RuntimeError, "image options"):
            setup("--anvil-image", "anvil:custom", directory=("--dir", str(self.fork.directory)))
        self.assertEqual(len(commands), 5)

    def test_completed_setup_rejects_image_options_that_differ_from_its_pins(self):
        work, image = self.root / "work", "sha256:" + "b" * 64
        config = {"sequencer_datadir": str(work / "builder"), "validator_datadir": str(work / "validator"),
                  "port": 19545, **{role + "_image": image for role in devnet.DEFAULT_IMAGES}}
        requested = {"anvil": "anvil:custom", "batcher": "op-batcher:custom"}
        journal = {"version": 1, "phase": "initialize", "download_container": "unused",
                   "images": {role: image for role in devnet.DEFAULT_IMAGES}, "requested_images": requested}
        fork = devnet.SnapshotFork(work / "fork")
        fork.directory.mkdir(parents=True)
        fork.manifest = {**manifest(), "setup_input": config}
        fork.save()
        devnet.write_json(fork.directory / "upstreams.json", {"execution": "https://rpc.invalid/key",
                                                              "beacon": "https://rpc.invalid/key"})
        devnet.write_json(work / "input.json", config)
        changed = ["--anvil-image", "anvil:other", "--batcher-image", "op-batcher:other"]
        matching = ["--anvil-image", "anvil:custom", "--batcher-image", "op-batcher:custom"]

        def setup(*options):
            saved = {path: path.read_bytes() for path in (work / "setup.json", work / "input.json",
                                                          fork.directory / "manifest.json", fork.directory / "upstreams.json")
                     if path.exists()}
            with patch.object(sys, "argv", ["launcher", "setup", "--workdir", str(work), *options]), \
                    patch.object(devnet.getpass, "getpass", side_effect=AssertionError("saved endpoints must not prompt")), \
                    patch.object(devnet, "rpc", return_value="0x1"), \
                    patch.object(devnet, "request_json", return_value={"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}), \
                    patch.object(devnet.shutil, "which", return_value="/usr/bin/tool"), \
                    patch.object(devnet, "run", side_effect=AssertionError("must not inspect or pull images")), \
                    patch.object(devnet, "setup_command", side_effect=AssertionError("must not build, download or copy")), \
                    patch.object(devnet.SnapshotFork, "initialize", side_effect=AssertionError("must not reinitialize")), \
                    patch("builtins.print"):
                try:
                    devnet.main()
                finally:
                    self.assertEqual({path: path.read_bytes() for path in saved}, saved, "pins must not change")

        for recorded in ("requested", "legacy journal", "no journal"):
            with self.subTest(recorded=recorded):
                if recorded == "no journal":
                    (work / "setup.json").unlink()
                else:
                    devnet.write_json(work / "setup.json", journal if recorded == "requested" else
                                      {key: value for key, value in journal.items() if key != "requested_images"})
                devnet.setup_path().unlink(missing_ok=True)
                # Unknown provenance cannot verify any explicit option.
                with self.assertRaisesRegex(RuntimeError, "image options"):
                    setup(*changed)
                self.assertFalse(devnet.setup_path().exists())
                if recorded != "requested":
                    with self.assertRaisesRegex(RuntimeError, "image options"):
                        setup(*matching)
                for options in ([], matching) if recorded == "requested" else ([],):
                    setup(*options)
                    self.assertEqual(devnet.configured_directory(), fork.directory)

    @unittest.skipUnless(shutil.which("rsync"), "requires rsync, but copies only temporary data")
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
                patch.object(devnet, "request_json", side_effect=lambda url, path: {
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

    def test_setup_builds_missing_defaults_pulls_remote_and_preserves_custom_images(self):
        image_id = "sha256:" + "f" * 64
        for source in ("cached", "missing", "remote", "custom", "pinned"):
            with self.subTest(source=source):
                work = self.root / source
                work.mkdir()
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
                        devnet.prepare_snapshot(args, devnet.SnapshotFork(work / "fork"), work, lock)
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

    @contextlib.contextmanager
    def preparing(self, command, initialize, fetched):
        """Mocked tools; later snapshot index reads offer a newer snapshot than the first."""
        def metadata(url, path=None):
            fetched.append(url)
            if path is not None:
                return {"data": {"genesis_time": "1000", "SECONDS_PER_SLOT": "12"}}
            if url == devnet.SNAPSHOT_INDEX:
                block = 122 + fetched.count(url)
                return [{"chainId": "8453", "block": str(block), "metadataUrl": f"https://snapshot.invalid/{block}/manifest.json"},
                        {"chainId": 8453, "block": 90, "metadataUrl": "https://snapshot.invalid/90/manifest.json"},
                        {"chainId": 1, "block": 999, "metadataUrl": "https://snapshot.invalid/999/manifest.json"}]
            return {"chain_id": 8453, "block": int(url.split("/")[-2])}

        with patch.object(devnet.shutil, "which", return_value="/usr/bin/tool"), \
                patch.object(devnet, "run", return_value="sha256:" + "e" * 64), \
                patch.object(devnet, "request_json", side_effect=metadata), \
                patch.object(devnet, "setup_command", side_effect=command), \
                patch.object(devnet.SnapshotFork, "initialize", autospec=True, side_effect=initialize), \
                patch("builtins.print"):
            yield

    @unittest.skipUnless(shutil.which("rsync"), "requires rsync, but copies only temporary data")
    def test_prepare_pins_snapshot_before_download_and_fully_copies_validator(self):
        work = self.root / "experiment with spaces"
        work.mkdir()
        image = "sha256:" + "e" * 64
        args = Mock(anvil_image=devnet.DEFAULT_IMAGES["anvil"], batcher_image=devnet.DEFAULT_IMAGES["batcher"],
                    download_concurrency=devnet.DEFAULT_DOWNLOAD_CONCURRENCY)
        commands, copied, fetched = [], [], []
        pinned = {"chain_id": 8453, "block": 123, "base_url": "https://snapshot.invalid/123/"}

        def command(*args, lock_fd):
            self.assertEqual(lock_fd, lock.fileno())
            commands.append(args)
            if args[:2] == ("docker", "run"):
                state = json.loads((work / "setup.json").read_text())
                self.assertEqual(state["phase"], "download")
                self.assertEqual(json.loads((work / "download-manifest.json").read_text()), pinned)
                self.assertEqual(args[args.index("--name") + 1], state["download_container"])
                self.assertIn(image, args)
                self.assertEqual(args[args.index("--manifest-path") + 1], "/work/download-manifest.json")
                self.assertEqual(args[args.index("--download-concurrency") + 1], "16")
                self.assertNotIn("--full", args)
                self.assertNotIn("--force", args)
                for flag in ("--with-txs-distance", "--with-receipts-distance", "--with-state-history-distance"):
                    self.assertEqual(args[args.index(flag) + 1], "1339200")
                (work / "builder/db").mkdir(parents=True)
                (work / "builder/db/mdbx.dat").write_bytes(b"snapshot-state")
                (work / "builder/static_files").mkdir()
                (work / "builder/static_files/headers").write_bytes(b"snapshot-history")
            elif args[0] == "rsync":
                copied.append(subprocess.run(args, check=True, capture_output=True, text=True).stdout)

        def initialize(fork, config, allow_write):
            self.assertTrue(allow_write)
            self.assertEqual(config, json.loads((work / "input.json").read_text()))
            for role in ("builder", "validator"):
                self.assertEqual((work / role / "static_files/headers").read_bytes(), b"snapshot-history")
            self.assertNotEqual((work / "builder/db/mdbx.dat").stat().st_ino,
                                (work / "validator/db/mdbx.dat").stat().st_ino)
            (work / "validator/db/mdbx.dat").write_bytes(b"validator-only-write")
            self.assertEqual((work / "builder/db/mdbx.dat").read_bytes(), b"snapshot-state")

        with tempfile.TemporaryFile() as lock, self.preparing(command, initialize, fetched):
            devnet.prepare_snapshot(args, devnet.SnapshotFork(work / "fork"), work, lock)
        self.assertEqual([args[:3] for args in commands if args[0] != "rsync"],
                         [("docker", "buildx", "bake"), ("cargo", "build", "--locked"), ("docker", "run", "--rm")])
        for option in ("etc/docker/docker-bake.hcl", "base.args.PROFILE=release", "--load"):
            self.assertIn(option, commands[0], "always build this checkout's Base before downloading")
        self.assertEqual(fetched, [devnet.SNAPSHOT_INDEX, "https://snapshot.invalid/123/manifest.json"])
        self.assertIn("100%", copied[0], "the copy must report progress")
        state = json.loads((work / "setup.json").read_text())
        self.assertEqual({key: state[key] for key in ("version", "phase", "images", "inspector_built")},
                         {"version": 1, "phase": "initialize", "inspector_built": True,
                          "images": {role: image for role in ("base", "anvil", "batcher")}})
        config = json.loads((work / "input.json").read_text())
        self.assertEqual(config, {"sequencer_datadir": str(work / "builder"), "validator_datadir": str(work / "validator"),
                                  "port": config["port"], **{role + "_image": image for role in ("base", "anvil", "batcher")}})
        self.assertGreaterEqual(config["port"], 1024)

    @unittest.skipUnless(shutil.which("rsync"), "requires rsync, but copies only temporary data")
    def test_prepare_resumes_each_interruption_without_repeating_completed_steps(self):
        write_json = devnet.write_json
        for failure in ("build", "inspector", "pin", "download", "copy", "initialize"):
            with self.subTest(failure=failure):
                work = self.root / failure
                work.mkdir()
                args = Mock(anvil_image="sha256:" + "a" * 64, batcher_image="sha256:" + "b" * 64)
                calls, fetched = [], []
                interrupted = False

                def step(phase):
                    nonlocal interrupted
                    calls.append(phase)
                    if phase == failure and not interrupted:
                        interrupted = True
                        raise KeyboardInterrupt()

                def command(*args, lock_fd):
                    if args[:3] == ("docker", "buildx", "bake"):
                        step("build")
                    elif args[0] == "cargo":
                        step("inspector")
                    elif args[:2] == ("docker", "run"):
                        self.assertEqual(json.loads((work / "download-manifest.json").read_text())["block"], 123)
                        (work / "builder/db").mkdir(parents=True, exist_ok=True)
                        (work / "builder/db/mdbx.dat").write_bytes(b"partial")
                        step("download")
                        (work / "builder/db/mdbx.dat").write_bytes(b"snapshot")
                    elif args[0] == "rsync":
                        (work / "validator/db").mkdir(parents=True, exist_ok=True)
                        (work / "validator/db/mdbx.dat").write_bytes(b"partial-copy")
                        step("copy")
                        subprocess.run(args, check=True, capture_output=True)

                def initialize(fork, config, allow_write):
                    # A resumed initialization must see its own writes, never a fresh copy.
                    self.assertEqual((work / "validator/db/mdbx.dat").read_bytes(),
                                     b"opened-by-inspection" if fork.manifest else b"snapshot")
                    fork.manifest = {**manifest(), "phase": "inspecting", "setup_input": config}
                    fork.save()
                    (work / "validator/db/mdbx.dat").write_bytes(b"opened-by-inspection")
                    step("initialize")

                def journal(path, value):
                    # "pin": the snapshot manifest is saved but the journal has not yet advanced.
                    if path.name == "setup.json" and value["phase"] == "download":
                        step("pin")
                    write_json(path, value)

                (work / "fork").mkdir()
                with tempfile.TemporaryFile() as lock, self.preparing(command, initialize, fetched), \
                        patch.object(devnet, "write_json", side_effect=journal):
                    with self.assertRaises(KeyboardInterrupt):
                        devnet.prepare_snapshot(args, devnet.SnapshotFork(work / "fork"), work, lock)
                    state = json.loads((work / "setup.json").read_text())
                    self.assertEqual(state["phase"], {"inspector": "build", "pin": "build"}.get(failure, failure))
                    self.assertEqual(state["images"]["base"].startswith("sha256:"), failure != "build")
                    self.assertEqual(state.get("inspector_built", False), failure not in ("build", "inspector"))
                    if failure in ("download", "copy"):
                        self.assertEqual((work / "builder/db/mdbx.dat").read_bytes(),
                                         b"partial" if failure == "download" else b"snapshot")
                    devnet.prepare_snapshot(args, devnet.SnapshotFork(work / "fork"), work, lock)
                self.assertEqual(calls, [phase for phase in ("build", "inspector", "pin", "download", "copy", "initialize")
                                         for _ in range(2 if phase == failure else 1)])
                self.assertEqual(fetched.count(devnet.SNAPSHOT_INDEX), 1, "retry must not select a newer snapshot")
                self.assertEqual(json.loads((work / "download-manifest.json").read_text())["block"], 123)
                self.assertEqual((work / "builder/db/mdbx.dat").read_bytes(), b"snapshot")
                self.assertEqual((work / "validator/db/mdbx.dat").read_bytes(), b"opened-by-inspection")

    def test_prepare_rejects_unsafe_saved_state_before_any_effect(self):
        image = "sha256:" + "e" * 64
        images = {role: image for role in ("base", "anvil", "batcher")}
        journal = {"version": 1, "phase": "copy", "images": images, "download_container": "snapshot-download-x"}
        args = Mock(anvil_image=None, batcher_image=None)
        cases = {
            "unknown phase": ({**journal, "phase": "verify"}, True, "unsupported snapshot setup journal"),
            "future journal": ({**journal, "version": 2}, True, "unsupported snapshot setup journal"),
            "missing input": (journal, False, "saved setup input is missing"),
            "unjournaled input": (None, True, "unrecorded download"),
            "changed image": ({**journal, "images": {**images, "anvil": "sha256:" + "f" * 64}}, True, "input changed"),
            "moved datadir": (journal, {"validator_datadir": "/elsewhere"}, "input changed"),
            "initialized": (journal, True, "initialization already began"),
            "symlinked datadir": (journal, True, "must not be symlinks"),
            "new symlinked datadir": (None, False, "must not be symlinks"),
            "missing rsync": (journal, True, "rsync"),
        }
        for name, (state, saved_input, error) in cases.items():
            with self.subTest(name=name):
                work = self.root / name
                fork = devnet.SnapshotFork(work / "fork")
                builder = self.datadir(f"{name}/builder")
                if state is not None:
                    devnet.write_json(work / "setup.json", state)
                if saved_input:
                    devnet.write_json(work / "input.json", {
                        "sequencer_datadir": str(builder), "validator_datadir": str(work / "validator"), "port": 30303,
                        **{role + "_image": image for role in images}, **(saved_input if isinstance(saved_input, dict) else {})})
                if name == "initialized":
                    fork.manifest = {**manifest(), "phase": "inspecting"}
                if "symlinked" in name:
                    (work / "validator").symlink_to(builder)
                before = {path: path.read_bytes() for path in work.rglob("*") if path.is_file()}
                with tempfile.TemporaryFile() as lock, \
                        patch.object(devnet.shutil, "which", side_effect=lambda tool: None if tool == "rsync" and name == "missing rsync" else "/usr/bin/tool"), \
                        patch.object(devnet, "run") as run, patch.object(devnet, "request_json") as fetch, \
                        patch.object(devnet, "setup_command") as command, \
                        patch.object(devnet.SnapshotFork, "initialize") as initialize:
                    with self.assertRaisesRegex(RuntimeError, error):
                        devnet.prepare_snapshot(args, fork, work, lock)
                for mock in (run, fetch, command, initialize):
                    mock.assert_not_called()
                self.assertEqual({path: path.read_bytes() for path in work.rglob("*") if path.is_file()}, before)

    def test_prepare_never_echoes_malformed_snapshot_metadata(self):
        work = self.root / "work"
        args = Mock(anvil_image="sha256:" + "a" * 64, batcher_image="sha256:" + "b" * 64)
        entry = {"chainId": 8453, "block": 123, "metadataUrl": "https://snapshot.invalid/123/manifest.json"}
        for name, index, metadata in (
                ("index chain", [{**entry, "chainId": "secret-token"}], {}),
                ("index block", [{**entry, "block": "secret-token"}], {}),
                ("manifest chain", [entry], {"chain_id": "secret-token", "block": 123}),
                ("manifest block", [entry], {"chain_id": 8453, "block": "secret-token"})):
            with self.subTest(name=name):
                shutil.rmtree(work, ignore_errors=True)
                work.mkdir()
                with tempfile.TemporaryFile() as lock, \
                        patch.dict(os.environ, {"BASE_SNAPSHOT_INSPECTOR": str(self.root / "base-devnet")}), \
                        patch.object(devnet.shutil, "which", return_value="/usr/bin/tool"), \
                        patch.object(devnet, "run", return_value="sha256:" + "e" * 64), \
                        patch.object(devnet, "request_json", side_effect=[index, metadata]), \
                        patch.object(devnet, "setup_command"), patch("builtins.print"):
                    (self.root / "base-devnet").touch()
                    with self.assertRaises(ValueError) as caught:
                        devnet.prepare_snapshot(args, devnet.SnapshotFork(work / "fork"), work, lock)
                self.assertNotIn("secret", str(caught.exception))
                self.assertFalse((work / "download-manifest.json").exists())

    def test_setup_commands_stream_progress_without_urls_and_report_failure(self):
        program = ("import sys; print('downloading 40% https://archive.invalid/x?X-Amz-Signature=secret-sig', flush=True); "
                   "print('progress 1/2 files', file=sys.stderr, flush=True); sys.exit(int(sys.argv[1]))")
        for status in (0, 3):
            with self.subTest(status=status), tempfile.TemporaryFile() as lock, \
                    contextlib.redirect_stdout(io.StringIO()) as output:
                try:
                    devnet.setup_command(sys.executable, "-c", program, status, lock_fd=lock.fileno())
                except RuntimeError as error:
                    self.assertTrue(status, error)
                    self.assertIn("setup command failed", str(error))
                else:
                    self.assertFalse(status, "a failing command must be reported")
            self.assertNotIn("secret", output.getvalue())
            self.assertNotIn("archive.invalid", output.getvalue())
            self.assertIn("downloading 40%", output.getvalue())
            self.assertIn("progress 1/2 files", output.getvalue())

    @unittest.skipUnless(shutil.which("rsync"), "requires rsync, but copies only temporary data")
    def test_interrupted_copy_resumes_from_its_partial_database(self):
        work, shims = self.root / "work", self.root / "shims"
        (work / "builder/db").mkdir(parents=True)
        source = os.urandom(1 << 22)
        (work / "builder/db/mdbx.dat").write_bytes(source)
        (work / "builder/reth.toml").write_text("complete\n")
        devnet.write_json(work / "setup.json", {"version": 1, "phase": "copy", "download_container": "unused",
                                                "images": {role: "sha256:" + "e" * 64 for role in devnet.DEFAULT_IMAGES}})
        devnet.write_json(work / "input.json", {"sequencer_datadir": str(work / "builder"),
                                                "validator_datadir": str(work / "validator"), "port": 30303,
                                                **{role + "_image": "sha256:" + "e" * 64 for role in devnet.DEFAULT_IMAGES}})
        # The first copy is throttled and interrupted once a database-sized file is partly written;
        # the retry reports how much of it rsync reused.
        shims.mkdir()
        (shims / "rsync").write_text(f"""#!{sys.executable}
import os, pathlib, signal, subprocess, sys, time
rsync = {shutil.which("rsync")!r}
database = pathlib.Path({str(work / "validator/db")!r})
if os.path.exists({str(shims / "interrupted")!r}):
    os.execv(rsync, [rsync, "--stats", *sys.argv[1:]])
pathlib.Path({str(shims / "interrupted")!r}).touch()
child = subprocess.Popen([rsync, "--bwlimit=256", *sys.argv[1:]])
deadline = time.monotonic() + 30
while not any(path.is_file() and path.stat().st_size >= 1 << 18
              for path in (database.iterdir() if database.is_dir() else ())):
    if time.monotonic() > deadline or child.poll() is not None:
        sys.exit("copy finished before it could be interrupted")
    time.sleep(0.01)
child.send_signal(signal.SIGTERM)
sys.exit(child.wait() or 1)
""")
        (shims / "rsync").chmod(0o700)
        args = Mock(anvil_image=None, batcher_image=None)
        with tempfile.TemporaryFile() as lock, patch.dict(os.environ, {"PATH": f"{shims}:{os.environ['PATH']}"}), \
                patch.object(devnet.SnapshotFork, "initialize") as initialize, patch("builtins.print") as output:
            with self.assertRaisesRegex(RuntimeError, "setup command failed"):
                devnet.prepare_snapshot(args, devnet.SnapshotFork(work / "fork"), work, lock)
            # The staging copy is written in place; nothing else holds the interrupted data.
            copy = work / "validator/db/mdbx.dat"
            self.assertEqual(os.listdir(work / "validator/db"), ["mdbx.dat"])
            partial, inode = copy.read_bytes(), copy.stat().st_ino
            self.assertTrue(0 < len(partial) < len(source) and source.startswith(partial))
            initialize.assert_not_called()
            output.reset_mock()
            devnet.prepare_snapshot(args, devnet.SnapshotFork(work / "fork"), work, lock)
            initialize.assert_called_once()
        printed = "".join(str(call.args[0]) for call in output.call_args_list if call.args)
        reused = int(re.search(r"Matched data: ([\d,]+)", printed)[1].replace(",", ""))
        self.assertGreaterEqual(reused, len(partial) - (1 << 16), printed)
        self.assertEqual(copy.read_bytes(), source)
        # The same inode was completed: the retry wrote no second database-sized file to rename into place.
        self.assertEqual(copy.stat().st_ino, inode)
        self.assertEqual(copy.stat().st_nlink, 1)
        self.assertNotEqual(copy.stat().st_ino, (work / "builder/db/mdbx.dat").stat().st_ino)
        self.assertEqual((work / "builder/db/mdbx.dat").read_bytes(), source)
        self.assertEqual(os.listdir(work / "validator/db"), ["mdbx.dat"])
        self.assertEqual(json.loads((work / "setup.json").read_text())["phase"], "initialize")

    def test_concurrent_setups_each_pin_their_own_base_build(self):
        # Checkouts share Docker's tag namespace; a build finishing later must not replace another's pin.
        tags, builds = {}, []
        inspector = self.root / "base-devnet"
        inspector.touch()

        def command(*command, lock_fd):
            if command[:3] == ("docker", "buildx", "bake"):
                settings = [command[index + 1] for index, arg in enumerate(command) if arg == "--set"]
                build = f"sha256:{len(builds) + 1:064x}"
                builds.append(build)
                tags.update(dict.fromkeys([setting.removeprefix("base.tags=") for setting in settings
                                           if setting.startswith("base.tags=")] or ["base:local"], build))
                if len(builds) == 1:
                    prepare(second)  # Another checkout builds before this one inspects its result.

        def docker(*command, **_):
            return tags[command[-1]] if command[-1] in tags else "sha256:" + "e" * 64

        def prepare(work):
            work.mkdir()
            with self.assertRaises(KeyboardInterrupt):  # Stop once the build phase is journaled.
                devnet.prepare_snapshot(Mock(anvil_image=devnet.DEFAULT_IMAGES["anvil"],
                                             batcher_image=devnet.DEFAULT_IMAGES["batcher"]),
                                        devnet.SnapshotFork(work / "fork"), work, lock)

        first, second = self.root / "first", self.root / "second"
        with tempfile.TemporaryFile() as lock, patch.dict(os.environ, {"BASE_SNAPSHOT_INSPECTOR": str(inspector)}), \
                patch.object(devnet.shutil, "which", return_value="/usr/bin/docker"), \
                patch.object(devnet, "run", side_effect=docker), \
                patch.object(devnet, "request_json", side_effect=KeyboardInterrupt), \
                patch.object(devnet, "setup_command", side_effect=command), patch("builtins.print"):
            prepare(first)
        self.assertEqual([json.loads((work / "setup.json").read_text())["images"]["base"] for work in (first, second)],
                         builds)

    def test_setup_builds_the_inspector_where_the_launcher_runs_it(self):
        # An inherited CARGO_TARGET_DIR would otherwise leave a stale or unrelated default inspector.
        work, calls = self.root / "work", []
        work.mkdir()
        with tempfile.TemporaryFile() as lock, patch.dict(os.environ, {"CARGO_TARGET_DIR": str(self.root / "elsewhere")}), \
                patch.object(devnet.shutil, "which", return_value="/usr/bin/docker"), \
                patch.object(devnet, "run", return_value="sha256:" + "e" * 64), \
                patch.object(devnet, "setup_command", side_effect=lambda *command, lock_fd: calls.append(command)), \
                patch.object(devnet, "request_json", side_effect=KeyboardInterrupt), patch("builtins.print"), \
                self.assertRaises(KeyboardInterrupt):
            os.environ.pop("BASE_SNAPSHOT_INSPECTOR", None)
            devnet.prepare_snapshot(Mock(anvil_image=devnet.DEFAULT_IMAGES["anvil"],
                                         batcher_image=devnet.DEFAULT_IMAGES["batcher"]),
                                    devnet.SnapshotFork(work / "fork"), work, lock)
        cargo = next(command for command in calls if command[0] == "cargo")
        self.assertEqual(cargo[cargo.index("--target-dir") + 1], "target")

    def test_setup_commands_inherit_the_fork_lock(self):
        with self.assertRaises(TypeError):
            devnet.setup_command(sys.executable, "-c", "pass")  # Every setup command must inherit the lock.
        with open(self.fork.directory / ".lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            devnet.setup_command(
                sys.executable, "-c",
                "import os, sys; assert os.fstat(int(sys.argv[1])).st_ino == int(sys.argv[2])",
                str(lock.fileno()), str(os.fstat(lock.fileno()).st_ino), lock_fd=lock.fileno())

    def test_first_start_runs_every_gate_in_order_before_sequencing_and_catch_up(self):
        calls = []
        with self.starting(), patch.multiple(self.fork, wait_checkpoints=DEFAULT, wait_boundary=DEFAULT,
                                             bootstrap=DEFAULT, mine=DEFAULT, start_batcher=DEFAULT,
                                             schedule_denim=DEFAULT), \
                patch.object(self.fork, "sync_status", return_value=sync_status(101)), \
                patch.object(devnet, "rpc", side_effect=lambda url, method, *args: calls.append(method) or (
                    {"hash": "0x123"} if method == "eth_getBlockByNumber" else method != "admin_sequencerActive")), \
                patch("builtins.print"):
            for name in ("await_rpc", "assert_local_l1", "validate_restored_contracts", "inspect", "wait_checkpoints",
                         "wait_boundary", "bootstrap", "wait_upgrades", "mine", "peers", "start_batcher",
                         "schedule_denim"):
                getattr(self.fork, name).side_effect = lambda *args, name=name: calls.append(name) or (
                    [] if name == "inspect" else None)
            self.fork.compose.side_effect = lambda *args: calls.append(args)
            self.fork.start()
        self.assertEqual(calls, [
            ("up", "-d", "--no-build", "l1"), "await_rpc", "assert_local_l1", "validate_restored_contracts", "inspect",
            ("up", "-d", "--no-build", "sequencer", "validator"), "await_rpc", "await_rpc", "admin_sequencerActive",
            "wait_checkpoints", "wait_boundary", "bootstrap", ("stop", "sequencer", "validator"),
            ("up", "-d", "--no-build", "sequencer", "validator"), "await_rpc", "await_rpc", "wait_upgrades",
            "mine", "anvil_setIntervalMining", "peers", "miner_getMaxDASize", "eth_getBlockByNumber",
            "admin_startSequencer", "start_batcher", "schedule_denim"])
        stored = devnet.SnapshotFork(self.fork.directory).manifest
        self.assertEqual((stored["phase"], stored["bootstrapped"]), ("running", True))

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
            # A scheduling failure on an already-running fork leaves its services running.
            schedule.side_effect = RuntimeError("Denim receipt timed out")
            with self.assertRaisesRegex(RuntimeError, "receipt timed out"):
                self.fork.start()
            stop.assert_not_called()
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

    def test_first_start_failing_before_l1_serves_is_retryable_without_l1_state(self):
        calls, phases = [], []
        durable = lambda: devnet.SnapshotFork(self.fork.directory).manifest["phase"]
        self.fork.save()
        with self.starting(), patch("sys.stderr"):
            self.fork.compose.side_effect = lambda *args: calls.append(args)
            self.fork.await_rpc.side_effect = RuntimeError("timed out: l1 execution RPC")
            with self.assertRaisesRegex(RuntimeError, "l1 execution RPC"):
                self.fork.start()
            self.assertEqual(durable(), "prepared")
            self.assertFalse((self.fork.directory / "l1/anvil.json").exists())
            # Retrying needs no saved L1 state: this fork's Anvil never served.
            self.fork.await_rpc.side_effect = None
            self.fork.assert_local_l1.side_effect = lambda: phases.append(("identity", durable()))
            self.fork.validate_restored_contracts.side_effect = lambda: phases.append(("contracts", durable()))
            self.fork.inspect.side_effect = RuntimeError("inspection failed")
            with self.assertRaisesRegex(RuntimeError, "inspection failed"):
                self.fork.start()
            # Starting is durable once the served L1 matches the fork, before any local write.
            self.assertEqual(phases, [("identity", "prepared"), ("contracts", "starting")])
            self.assertEqual(calls.count(("up", "-d", "--no-build", "l1")), 2)
            # Once Anvil served this fork, its saved L1 state is required.
            calls.clear()
            with self.assertRaisesRegex(RuntimeError, "L1 state is missing"):
                self.fork.start()
            self.assertEqual(calls, [])

    def test_failed_start_attempts_every_cleanup_stage_and_keeps_the_original_error(self):
        dependents = ("stop", "batcher", "sequencer", "validator", "inspect-sequencer", "inspect-validator")
        for original in (RuntimeError("inspection failed"), KeyboardInterrupt()):
            for failing in (dependents, ("stop", "l1"), None):
                with self.subTest(original=original, failing=failing):
                    self.fork.manifest, calls = manifest(), []

                    def compose(*args):
                        calls.append(args)
                        if args == failing:
                            raise RuntimeError("docker timed out after 1s; data preserved")

                    with self.starting(), patch("sys.stderr", new_callable=io.StringIO) as stderr:
                        self.fork.compose.side_effect = compose
                        self.fork.inspect.side_effect = original
                        with self.assertRaises(type(original)) as raised:
                            self.fork.start()
                    self.assertIs(raised.exception, original)
                    self.assertEqual(calls[1:], [dependents, ("stop", "l1")])
                    if failing:
                        self.assertIn("docker timed out", stderr.getvalue())

    def test_resume_rejects_missing_l1_dump_before_starting_any_database(self):
        self.fork.manifest["phase"] = "stopped"
        self.fork.manifest["datadirs"] = {"sequencer": str(self.datadir("a")), "validator": str(self.datadir("b"))}
        with patch.object(self.fork, "running", return_value=False), patch.object(self.fork, "endpoint"), \
                patch.object(self.fork, "compose") as compose, patch.object(self.fork, "inspect") as inspect, \
                patch("sys.stderr"):
            with self.assertRaisesRegex(RuntimeError, "L1 state is missing"):
                self.fork.start()
            inspect.assert_not_called()
            compose.assert_not_called()
        self.assertEqual(self.fork.manifest["phase"], "stopped")

    def test_start_rejects_restored_l2_with_missing_local_l1_history_before_starting_nodes(self):
        calls = []
        with self.starting(), patch.object(self.fork, "inspect", return_value=[snapshot()]), \
                patch.object(devnet, "rpc", return_value=None), \
                patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)), patch("sys.stderr"):
            with self.assertRaisesRegex(RuntimeError, "missing/conflicting L1 history"):
                self.fork.start()
        self.assertEqual(calls, [("up", "-d", "--no-build", "l1"),
                                 ("stop", "batcher", "sequencer", "validator", "inspect-sequencer", "inspect-validator"),
                                 ("stop", "l1")])

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

                with self.starting(), \
                        patch.multiple(self.fork, start_batcher=DEFAULT, schedule_denim=DEFAULT), \
                        patch.object(self.fork, "mine", side_effect=lambda: enable("mine")), \
                        patch.object(self.fork, "sync_status", side_effect=status), \
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

                with self.starting(), \
                        patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)), \
                        patch.object(self.fork, "running_services", return_value={"l1", "sequencer", "validator"}), \
                        patch.object(self.fork, "mine"), \
                        patch.object(self.fork, "sync_status", return_value=sync_status(101)), \
                        patch.object(devnet, "wait", side_effect=waiting), \
                        patch.object(devnet, "rpc", side_effect=transport), patch("builtins.print"):
                    with self.assertRaisesRegex(RuntimeError, "miner" if missing_miner else "batcher"):
                        self.fork.start()
                self.assertNotEqual(devnet.SnapshotFork(self.fork.directory).manifest["phase"], "running")
                self.assertEqual(calls[-2:], [("stop", "batcher", "sequencer", "validator", "inspect-sequencer",
                                               "inspect-validator"), ("stop", "l1")])
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

                with self.starting(), patch.object(self.fork, "mine"), \
                        patch.object(self.fork, "schedule_denim", side_effect=lambda: calls.append("denim")), \
                        patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)), \
                        patch.object(self.fork, "running_services", side_effect=lambda: {
                            "l1", "sequencer", "validator"} | (set() if batcher_exits and polls else {"batcher"})), \
                        patch.object(self.fork, "start_batcher", side_effect=lambda: calls.append("batched")), \
                        patch.object(self.fork, "sync_status", side_effect=status), \
                        patch.object(devnet.time, "sleep") as sleep, \
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
                    self.assertEqual(calls[-2:], [("stop", "batcher", "sequencer", "validator", "inspect-sequencer",
                                                   "inspect-validator"), ("stop", "l1")])
                    self.assertNotIn("denim", calls)
                else:
                    self.assertIn("denim", calls, "up must schedule Denim without a separate manual step")
                    self.assertLess(calls.index("catch-up"), calls.index("denim"))
                    # Hours of catch-up must not query Docker every second.
                    sleep.assert_called_once_with(5)

    def test_restored_contracts_must_match_the_recorded_schedule_and_implementations(self):
        self.fork.manifest.update(schedule=[100] * 13, contracts={"0x1": "0ximplementation"})
        for schedule, implementation, pattern in (([100] * 13, "0ximplementation", None),
                                                  ([100] * 13 + [9998], "0ximplementation", "schedule differs"),
                                                  ([100] * 13, "0xother", "implementation differs")):
            with self.subTest(pattern=pattern), patch.object(devnet, "call", return_value=list(map(str, schedule))), \
                    patch.object(devnet, "rpc", return_value=implementation):
                if pattern is None:
                    self.fork.validate_restored_contracts()
                else:
                    with self.assertRaisesRegex(RuntimeError, pattern):
                        self.fork.validate_restored_contracts()
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

    def test_denim_defaults_to_earliest_timestamp_from_live_notice_and_latest_clock(self):
        # max(L1, L2, wall) + MIN_NOTICE + slot is the contract floor, plus one more slot. Odd timestamps
        # are valid: activation rounds forward to each chain's next block.
        for notice, now, cobalt, expected in ((3600, 1293.4, 800, 4919), (7200, 1293.4, 800, 8519),
                                              (3600, 1401, 800, 5025), (3600, 1293.4, 9001, 9001)):
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

    def test_denim_explicit_timestamp_keeps_notice_and_cobalt_validations(self):
        # Contract floor: max(1290, 1295, 1293) + 3600 + 12 = 4907.
        for timestamp, schedule, cobalt, error in (
            (4906, [], 800, "3600s notice"),
            (4908, [], 0, "Cobalt must already be scheduled"), (4908, [], 5000, "Cobalt must already be scheduled"),
            (4907, [], 800, None), (4908, [], 800, None), (4909, [0], 800, None),
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
                        self.assertEqual(fake["sent"], [("setTimestamp(uint256,uint64)", 13, timestamp) if schedule
                                                        else ("registerUpgrade(uint64,uint256)", timestamp, 0)])
                        self.assertEqual(self.fork.manifest["denim_timestamp"], timestamp)

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
            ("never sent", None, [], None, [("registerUpgrade(uint64,uint256)", 4919, 0)]),
            ("different", {"hash": "0xtx"}, [], 5000, "already pending at 4000"),
        ):
            with self.subTest(name):
                self.fork.manifest = manifest()
                # A never-sent journal holds a stale timestamp and is rescheduled at the earliest default,
                # 4919; the others already submitted 4920.
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
                self.assertEqual(stored["denim_timestamp"], 4919 if operation is None else 4920)
                self.assertNotIn("pending_denim_timestamp", stored)

    def test_denim_notice_starts_from_the_latest_chain_clock(self):
        # Floor = max(L1, L2, wall) + 3600 + 12; the default adds one slot.
        for l1_time, l2_time, cobalt, default, exact in ((1500, 1295, 800, 5124, 5112), (1290, 1501, 800, 5125, 5113),
                                                         (1290, 1296, 4908, 4920, 4908)):
            for timestamp, error in ((None, None), (exact - 1, "notice"), (exact, None)):
                with self.subTest(l1_time=l1_time, l2_time=l2_time, timestamp=timestamp):
                    self.fork.manifest = manifest()
                    with self.protocol_versions([], l1_time=l1_time, l2_time=l2_time, cobalt=cobalt) as fake:
                        if error:
                            with self.assertRaisesRegex(RuntimeError, error):
                                self.fork.schedule_denim(timestamp)
                            self.assertEqual(fake["sent"], [])
                        else:
                            self.fork.schedule_denim(timestamp)
                            self.assertEqual(fake["sent"], [("registerUpgrade(uint64,uint256)", timestamp or default, 0)])

    def test_denim_rejects_stopped_fork_and_unsupported_schedules_without_writes(self):
        for name, schedule, error in (("stopped", [], "start the fork"), ("everest", [4920, 6000], "no implicit"),
                                      ("historical", [], "historical schedule changed")):
            with self.subTest(name):
                self.fork.manifest = manifest()
                with self.protocol_versions(schedule) as fake:
                    if name == "stopped":
                        self.fork.manifest["phase"] = "stopped"
                    if name == "historical":
                        self.fork.manifest["schedule"][3] = 99
                    with self.assertRaisesRegex(RuntimeError, error):
                        self.fork.schedule_denim()
                self.assertEqual(fake["sent"], [])
                self.assertNotIn("pending_denim_timestamp", self.fork.manifest)
                self.assertNotIn("denim_timestamp", self.fork.manifest)

    def test_ambiguous_denim_submission_is_never_resent(self):
        owner = "0x" + "a" * 40
        for mined in (False, True):
            with self.subTest(mined=mined):
                self.fork.manifest = manifest()
                self.fork.manifest.update(phase="running", schedule=[100] * 12 + [800], pending_denim_timestamp=4920)
                # Journaled with its nonce, but the send was interrupted before its hash was saved.
                self.fork.manifest["operations"]["schedule-denim"] = {"transaction": {
                    "from": owner, "to": devnet.PROTOCOL_VERSIONS, "value": "0x0", "data": "0xcalldata", "nonce": "0x4"}}
                schedule = [100] * 12 + [800] + ([4920] if mined else [])
                contract = {"getSchedule()(uint64[])": schedule, "minimumProtocolVersion()(uint256)": 42,
                            "proxyAdminOwner()(address)": owner}
                with patch.object(self.fork, "assert_local_l1"), patch.object(self.fork, "url", side_effect=lambda role: role), \
                        patch.object(devnet, "call", side_effect=lambda url, address, signature, *a, **k: contract[signature]), \
                        patch.object(devnet, "run", return_value="0xcalldata"), \
                        patch.object(devnet, "rpc") as transport:
                    with self.assertRaisesRegex(RuntimeError, "reconcile its nonce"):
                        self.fork.schedule_denim()
                    if mined:
                        with self.assertRaisesRegex(RuntimeError, "reconcile its nonce"):
                            self.fork.validate_restored_contracts()
                    transport.assert_not_called()
                self.assertEqual(self.fork.manifest["pending_denim_timestamp"], 4920)
                self.assertNotIn("denim_timestamp", self.fork.manifest)

    def test_restored_pending_denim_requires_its_successful_receipt(self):
        for receipt in (None, {"status": "0x0"}):
            with self.subTest(receipt=receipt):
                self.fork.manifest = manifest()
                self.fork.manifest.update(schedule=[100] * 13, pending_denim_timestamp=9000)
                self.fork.manifest["operations"]["schedule-denim"] = {"hash": "0xtx"}
                with patch.object(devnet, "call", return_value=[100] * 13 + [9000]), \
                        patch.object(devnet, "rpc", return_value=receipt):
                    with self.assertRaisesRegex(RuntimeError, "missing from restored L1"):
                        self.fork.validate_restored_contracts()
                self.assertEqual(self.fork.manifest["pending_denim_timestamp"], 9000)
                self.assertNotIn("denim_timestamp", self.fork.manifest)

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

        with self.starting(), \
                patch.multiple(self.fork, wait_checkpoints=DEFAULT, mine=DEFAULT, start_batcher=DEFAULT), \
                patch.object(self.fork, "schedule_denim", side_effect=RuntimeError("Denim receipt timed out")), \
                patch.object(self.fork, "compose", side_effect=lambda *args: calls.append(args)), \
                patch.object(self.fork, "sync_status", return_value=sync_status(101)), \
                patch.object(devnet, "rpc", side_effect=transport), patch("builtins.print"):
            with self.assertRaisesRegex(RuntimeError, "running but Denim.*receipt timed out.*rerun schedule-denim"):
                self.fork.start()
        self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["phase"], "running")
        self.assertFalse([call for call in calls if call[0] == "stop"])

    def test_inspection_uses_the_recorded_schedule_and_persisted_denim_without_changing_the_config(self):
        config = {"genesis": {"l2_time": 100}, "base": {"cobalt": None}}
        (self.fork.directory / "config").mkdir()
        devnet.write_json(self.fork.directory / "config/rollup.json", config)
        self.fork.manifest.update(initial=[{"rollup_config": config}], schedule=[100] * 12 + [9000],
                                  denim_timestamp=9500)
        with patch.object(self.fork, "compose"), patch.object(self.fork, "await_rpc"), \
                patch.object(self.fork, "containers", return_value=[]), \
                patch.object(self.fork, "url", return_value="http://10.9.0.2:8545"), \
                patch.object(devnet, "run", return_value="{}") as inspect, patch("builtins.print"):
            self.fork.inspect()
            self.assertIn("--rollup-config", inspect.call_args.args)
            self.assertNotIn("--find-fork", inspect.call_args.args)
        inspection = json.loads((self.fork.directory / "config/inspection.json").read_text())
        self.assertEqual((inspection["base"]["cobalt"], inspection["base"]["denim"]), (9000, 9500))
        self.assertEqual(json.loads((self.fork.directory / "config/rollup.json").read_text()), config)

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

    def test_stop_orders_dependents_before_l1_and_never_removes_data(self):
        calls = []
        with patch.object(self.fork, "running", return_value=True), \
                patch.object(self.fork, "containers", return_value=[container("sequencer")]), \
                patch.object(self.fork, "consensus_ready", return_value=True), \
                patch.object(self.fork, "sync_status", return_value=sync_status(101)), \
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

    def test_stop_attempts_every_stage_despite_failures_then_reports_the_first(self):
        stages = [("stop", "batcher"), "record_checkpoints",
                  ("stop", "sequencer", "validator", "inspect-sequencer", "inspect-validator"), ("stop", "l1")]
        for failing in stages:
            with self.subTest(failing=failing):
                self.fork.manifest["phase"] = "running"
                calls = []

                def attempt(stage):
                    calls.append(stage)
                    if stage == failing:
                        raise RuntimeError("docker timed out after 1s; data preserved")

                with patch.object(self.fork, "running", return_value=True), \
                        patch.object(self.fork, "consensus_ready", return_value=False), \
                        patch.object(self.fork, "record_checkpoints", side_effect=lambda: attempt("record_checkpoints")), \
                        patch.object(self.fork, "compose", side_effect=lambda *args: attempt(args)):
                    with self.assertRaisesRegex(RuntimeError, "timed out"):
                        self.fork.stop()
                    self.assertEqual(calls, stages)
                    # Rerunning down attempts every stage again.
                    calls.clear()
                    failing = None
                    self.fork.stop()
                    self.assertEqual(calls, stages)
                self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["phase"], "stopped")

    def test_stop_before_first_start_keeps_prepared_fork_startable(self):
        with patch.object(self.fork, "running", return_value=False):
            self.fork.stop()
        self.assertEqual(devnet.SnapshotFork(self.fork.directory).manifest["phase"], "prepared")

    def test_reset_retires_only_a_stopped_confirmed_fork_and_keeps_datadirs(self):
        self.fork.save()
        datadir = self.datadir("sequencer")
        def reset(project):
            with patch.object(sys, "argv", ["launcher", "reset", "--dir", str(self.fork.directory),
                                           "--confirm-project", project]), patch("builtins.print"):
                devnet.main()
        with patch.object(devnet.SnapshotFork, "running", return_value=True):
            with self.assertRaisesRegex(RuntimeError, "confirmation does not match"):
                reset("snapshot-other")
            with self.assertRaisesRegex(RuntimeError, "stop the fork"):
                reset("snapshot-fixture")
        self.assertTrue((self.fork.directory / "manifest.json").is_file())
        with patch.object(devnet.SnapshotFork, "running", return_value=False):
            reset("snapshot-fixture")
        retired, = self.root.glob("fork.retired-*")
        self.assertEqual(json.loads((retired / "manifest.json").read_text()), self.fork.manifest)
        self.assertFalse(self.fork.directory.exists())
        self.assertEqual((datadir / "db/mdbx.dat").read_bytes(), b"untouched")

    def test_running_refreshes_container_state_without_compose_or_keys(self):
        self.fork._containers = []
        with patch.object(devnet, "run", side_effect=[
                "abc", json.dumps([container("inspect-validator")]),
                "abc", json.dumps([container("inspect-validator", running=False)]),
        ]):
            self.assertTrue(self.fork.running())
            self.assertFalse(self.fork.running())

    def test_compose_before_discovery_uses_inert_placeholders_and_cannot_start_the_fork(self):
        del self.fork.manifest["fork"], self.fork.manifest["slot_seconds"]
        devnet.write_json(self.fork.directory / "keys.json", {"signer": "0x1", "batcher": "0x2"})
        environment = self.fork.compose_env()
        self.assertFalse(environment["SNAPSHOT_FORK_BLOCK"].isdigit())
        self.assertFalse(environment["SNAPSHOT_SLOT_SECONDS"].isdigit())
        with patch.object(devnet, "run") as run, patch("builtins.print"):
            self.fork.compose("--profile", "inspect", "up", "-d", "--no-build", "inspect-sequencer", "inspect-validator")
            for service in ("l1", "sequencer", "validator", "batcher"):
                with self.assertRaisesRegex(RuntimeError, "before F"):
                    self.fork.compose("up", "-d", "--no-build", service)
            self.assertEqual(run.call_count, 1)

    @unittest.skipUnless(shutil.which("just"), "requires the just command dispatcher")
    def test_nested_just_commands_forward_arguments_without_starting_services(self):
        environment = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}
        for command in ("setup", "init", "up", "down", "start", "stop", "status", "reset", "schedule-denim",
                        "deposit", "verify"):
            with self.subTest(command=command):
                result = subprocess.run(
                    ["just", "devnet", "snapshot", command, "--dir", str(self.root / "fork with spaces"), "--help"],
                    cwd=devnet.ROOT, capture_output=True, text=True, timeout=10, env=environment)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("usage:", result.stdout)
                if command == "verify":
                    for flag in ("--denim", "--restart", "--interrupt"):
                        self.assertNotIn(flag, result.stdout)
                self.assertFalse((self.root / "fork with spaces").exists())
        result = subprocess.run(
            ["just", "devnet", "snapshot", "status", "--dir", str(self.root / "fork with spaces"), "--timeout", "1"],
            cwd=devnet.ROOT, capture_output=True, text=True, timeout=10, env=environment)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fork directory does not exist", result.stderr)

    @unittest.skipUnless(shutil.which("just"), "requires the just command dispatcher")
    def test_snapshot_is_a_just_module_that_forwards_init_arguments_without_writes(self):
        for args in (["devnet", "snapshot"], ["--list", "devnet", "snapshot"]):
            with self.subTest(args=args):
                result = subprocess.run(["just", *args], cwd=devnet.ROOT, capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                for command in ("setup", "init", "up", "down", "status", "reset", "schedule-denim", "test", "build-anvil",
                                "deposit", "verify"):
                    self.assertIn(command, result.stdout)
                self.assertNotIn("snapshot-verify", result.stdout)
        target = self.root / "fork with spaces"
        config_path = self.root / "input with spaces.json"
        devnet.write_json(config_path, self.preparation())
        environment = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}
        result = subprocess.run(["just", "devnet", "snapshot", "init", "--dir", str(target), "--help"],
                                cwd=devnet.ROOT, capture_output=True, text=True, timeout=10, env=environment)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--allow-write", result.stdout)
        self.assertFalse(target.exists())
        result = subprocess.run(["just", "devnet", "snapshot", "init", "--dir", str(target), "--config", str(config_path)],
                                cwd=devnet.ROOT, capture_output=True, text=True, timeout=10, env=environment)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("init requires --allow-write", result.stderr)
        self.assertEqual([entry.name for entry in target.iterdir()], [".lock"])


class QualificationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.fork = Mock(spec=devnet.SnapshotFork)
        self.fork.directory, self.fork.timeout = self.directory, 1
        self.fork.manifest = {**manifest(), "phase": "running"}
        self.fork.url.side_effect = lambda role: role
        sleep = patch.object(devnet.time, "sleep")
        sleep.start()
        self.addCleanup(sleep.stop)

    def test_verify_funds_through_l1_then_requires_safe_derivation_and_local_blobs(self):
        receipt = {"blockNumber": "0x7c", "blockHash": "0x124", "transactionHash": "0xtx", "status": "0x1"}
        safe = {"number": "0x7c", "hash": "0x124", "stateRoot": "0xroot"}
        for failure in ("derivation", None):
            with self.subTest(failure=failure):
                self.fork.reset_mock()
                # The deposit reaches L2 on the second poll; L1 advances while the transaction is batched.
                responses = {"eth_getBalance": iter([hex(0), hex(10**18)]), "eth_blockNumber": iter([hex(149), hex(152)])}
                calls = []

                def rpc(url, method, *args):
                    if method == "admin_sequencerActive":
                        return False
                    self.fork.deposit.assert_called_once_with(10**18)
                    calls.append(method)
                    return next(responses[method])

                with patch.object(verification, "rpc", side_effect=rpc), \
                        patch.object(verification, "transact", side_effect=lambda fork: calls.append("send") or receipt), \
                        patch.object(verification, "derived", return_value=safe,
                                     side_effect=RuntimeError("state root mismatch") if failure else None) as derived, \
                        patch.object(verification, "local_blobs", return_value=[{"path": "blob"}]) as blobs, \
                        patch("builtins.print") as output:
                    if failure:
                        with self.assertRaisesRegex(RuntimeError, "state root mismatch"):
                            verification.verify(self.fork)
                        self.assertFalse((self.directory / "verification.json").exists())
                        blobs.assert_not_called()
                        continue
                    verification.verify(self.fork)
                # The funded deposit is visible before sending; blobs are searched from the next L1 block.
                self.assertEqual(calls, ["eth_getBalance", "eth_getBalance", "eth_blockNumber", "send", "eth_blockNumber"])
                derived.assert_called_once_with(self.fork, receipt)
                blobs.assert_called_once_with(self.fork, 150, 152)
                report = json.loads((self.directory / "verification.json").read_text())
                self.assertEqual(report, {"blocks": [safe], "receipts": [receipt], "blobs": [{"path": "blob"}]})
                for method in ("stop", "start", "compose", "peers", "schedule_denim"):
                    getattr(self.fork, method).assert_not_called()
                self.assertNotIn("Denim", str(output.call_args_list))

    def test_verify_reuses_a_recorded_custom_deposit_and_never_skips_an_unresolved_submission(self):
        fork = devnet.SnapshotFork(self.directory)
        user, portal = manifest()["accounts"]["user"], "0x" + "e" * 40
        # As journaled by an earlier `deposit --wei 5`, not the verifier's default 1 ETH.
        recorded = {"from": user, "to": portal, "value": hex(5), "data": "0xcalldata", "nonce": "0x3", "gas": "0x1"}
        for operation, error in (({"transaction": recorded, "hash": "0xdeposit"}, "L2 transaction reached"),
                                 ({"transaction": recorded}, "reconcile its nonce manually")):
            fork.manifest = {**manifest(), "phase": "running", "portal": portal,
                             "operations": {"fund-user": copy.deepcopy(operation)}}

            def rpc(url, method, *args):
                responses = {"admin_sequencerActive": False, "eth_getBalance": hex(5), "eth_blockNumber": hex(149)}
                if method == "eth_getTransactionReceipt":
                    self.assertEqual((url, args), ("l1", ("0xdeposit",)))
                    return {"status": "0x1"}
                self.assertIn(method, responses, "a recorded deposit is reconciled, never sent again")
                return responses[method]

            with self.subTest(error=error), patch.multiple(fork, assert_local_l1=DEFAULT,
                                                           validate_restored_contracts=DEFAULT), \
                    patch.object(fork, "running_services", return_value=devnet.FORK_SERVICES), \
                    patch.object(fork, "url", side_effect=lambda role: role), \
                    patch.object(fork, "sync_status", return_value={"unsafe_l2": {"l1origin": {"number": 10}}}), \
                    patch.object(devnet, "run", return_value="0xcalldata"), \
                    patch.object(devnet, "rpc", side_effect=rpc), patch.object(verification, "rpc", side_effect=rpc), \
                    patch.object(verification, "transact", side_effect=RuntimeError("L2 transaction reached")) as send, \
                    patch("builtins.print"), self.assertRaisesRegex(RuntimeError, error):
                verification.verify(fork)
            self.assertEqual(fork.manifest["operations"]["fund-user"]["transaction"], recorded)
            self.assertEqual(send.called, "hash" in operation)

    def test_verify_rejects_a_transaction_whose_unsafe_block_was_replaced_before_it_became_safe(self):
        receipt = {"blockNumber": "0x7c", "blockHash": "0xsubmitted", "transactionHash": "0xtx", "status": "0x1"}
        # Both nodes agree at the receipt's height in every case; only the submitted hash proves inclusion.
        for block_hash, validator_receipt, error in (
                ("0xreplacement", None, "replaced"),
                ("0xreplacement", {**receipt, "blockHash": "0xreplacement"}, "replaced"),
                ("0xsubmitted", None, "validator lacks"),
                ("0xsubmitted", {**receipt, "status": "0x0"}, "validator lacks"),
                ("0xsubmitted", receipt, "derivation passed")):
            header = {"number": "0x7c", "hash": block_hash, "stateRoot": "0xroot"}
            responses = {"eth_getBalance": iter([hex(10**18)]), "eth_blockNumber": iter([hex(149), hex(152)])}

            def rpc(url, method, *args):
                if method == "admin_sequencerActive":
                    return False
                if method == "eth_getBlockByNumber":
                    return header
                if method == "eth_getTransactionReceipt":
                    self.assertEqual((url, args), ("validator", ("0xtx",)))
                    return validator_receipt
                return next(responses[method])

            with self.subTest(block_hash=block_hash, validator_receipt=validator_receipt), \
                    patch.object(verification, "rpc", side_effect=rpc), \
                    patch.object(verification, "transact", return_value=receipt), \
                    patch.object(verification, "local_blobs", side_effect=RuntimeError("derivation passed")), \
                    patch("builtins.print"), self.assertRaisesRegex(RuntimeError, error):
                self.fork.sync_status.return_value = {"safe_l2": {"number": 124}, "unsafe_l2": {"l1origin": {"number": 10}}}
                verification.verify(self.fork)

    def test_verify_rejects_stopped_fork_or_sequencing_validator_before_deposit(self):
        for phase, active in (("stopped", False), ("running", True)):
            with self.subTest(phase=phase, active=active):
                self.fork.manifest["phase"] = phase
                with patch.object(verification, "rpc", return_value=active), patch("builtins.print"):
                    with self.assertRaises(RuntimeError):
                        verification.verify(self.fork)
                self.fork.deposit.assert_not_called()

    def test_verify_fails_promptly_without_a_running_batcher(self):
        self.fork.require_batcher.side_effect = RuntimeError("batcher exited")
        with patch.object(verification, "rpc", return_value=False), patch("builtins.print"), \
                self.assertRaisesRegex(RuntimeError, "batcher exited"):
            verification.verify(self.fork)
        self.fork.deposit.assert_not_called()
        self.fork.sync_status.assert_not_called()

    def test_derivation_waits_for_safe_head_even_when_unsafe_has_advanced(self):
        self.fork.sync_status.side_effect = [
            {"safe_l2": {"number": 123}, "unsafe_l2": {"number": 130}},
            {"safe_l2": {"number": 124}, "unsafe_l2": {"number": 130}},
        ]
        header = {"number": "0x7c", "hash": "canonical", "stateRoot": "root"}
        receipt = {"blockNumber": "0x7c", "blockHash": "canonical", "transactionHash": "0xtx", "status": "0x1"}

        def rpc(url, method, *args):
            self.assertEqual(self.fork.sync_status.call_count, 2, "unsafe gossip is not proof of derivation")
            if method == "eth_getTransactionReceipt":
                return receipt
            self.assertEqual((method, *args), ("eth_getBlockByNumber", "0x7c", False))
            return header

        with patch.object(verification, "rpc", side_effect=rpc), patch("builtins.print"):
            self.assertEqual(verification.derived(self.fork, receipt), header)

    def test_matching_height_is_not_parity(self):
        with self.assertRaisesRegex(RuntimeError, "state root mismatch"):
            verification.check_parity({"hash": "a", "stateRoot": "a"}, {"hash": "a", "stateRoot": "b"})
        with self.assertRaisesRegex(RuntimeError, "hash/state root mismatch"):
            verification.check_parity({"hash": "a", "stateRoot": "a"}, {"hash": "b", "stateRoot": "a"})

    def test_transaction_must_succeed_on_the_sequencer(self):
        devnet.write_json(self.directory / "keys.json", {"user": "0x" + "1" * 64})
        for status in ("0x1", "0x0"):
            with self.subTest(status=status), patch.object(verification, "run", return_value="0xtx") as send, \
                    patch.object(verification, "rpc", return_value={"status": status}) as receipt, patch("builtins.print"):
                if status == "0x0":
                    with self.assertRaisesRegex(RuntimeError, "reverted"):
                        verification.transact(self.fork)
                else:
                    self.assertEqual(verification.transact(self.fork), {"status": status})
                self.assertEqual(send.call_args.args[send.call_args.args.index("--rpc-url") + 1], "sequencer")
                receipt.assert_called_with("sequencer", "eth_getTransactionReceipt", "0xtx")

    def test_local_blobs_require_batcher_blob_bytes_from_local_beacon(self):
        batcher = self.fork.manifest["accounts"]["batcher"]
        headers = {150: {"timestamp": hex(1060), "transactions": [
                       {"from": "0x" + "9" * 40, "hash": "0xother", "blobVersionedHashes": ["0xh0"]},
                       {"from": batcher.upper(), "hash": "0xcalldata"}]},
                   151: {"timestamp": hex(1072), "transactions": [
                       {"from": batcher.upper(), "hash": "0xbatch", "blobVersionedHashes": ["0xh1", "0xh2"]}]}}
        path = "/eth/v1/beacon/blobs/6?versioned_hashes=0xh1,0xh2"
        # The beacon filter silently omits unknown hashes, so one blob for two hashes is incomplete.
        for payload, error in (({"data": [{"blob": "0x1"}, {"blob": "0x2"}]}, None), ({"data": []}, "unavailable"),
                               ({"data": [{"blob": "0x1"}]}, "unavailable"),
                               ({"data": [{"blob": "0x1"}, None]}, "unavailable"), (None, "no locally posted blob batch")):
            # Block 150 has only another sender's blobs and a batcher calldata transaction.
            last = 150 if payload is None else 151
            with self.subTest(payload=payload), \
                    patch.object(verification, "rpc", side_effect=lambda url, method, height, full: headers[int(height, 16)]), \
                    patch.object(verification, "request_json", return_value=payload) as beacon:
                if error:
                    with self.assertRaisesRegex(RuntimeError, error):
                        verification.local_blobs(self.fork, 150, last)
                    continue
                result = verification.local_blobs(self.fork, 150, last)
            beacon.assert_called_once_with("l1" + path)
            self.assertEqual([(item["path"], item["l1_block"], item["transaction"]) for item in result],
                             [(path, 151, "0xbatch")])

    def test_deposit_funds_the_user_once_through_the_l1_portal(self):
        fork = devnet.SnapshotFork(self.directory)
        fork.manifest = {**manifest(), "portal": "0x" + "e" * 40}
        fork.save()
        user = fork.manifest["accounts"]["user"]
        with patch.object(fork, "send", return_value={"status": "0x1"}) as send:
            for amount in (0, -1):
                with self.assertRaisesRegex(RuntimeError, "positive"):
                    fork.deposit(amount)
            send.assert_not_called()
            self.assertEqual(fork.deposit(5), {"status": "0x1"})
        send.assert_called_once_with("fund-user", user, "0x" + "e" * 40,
                                     "depositTransaction(address,uint256,uint64,bool,bytes)",
                                     user, 5, 100000, "false", "0x", value=5)
        with patch.object(sys, "argv", ["launcher", "deposit", "--dir", str(self.directory), "--wei", "7"]), \
                patch.object(devnet.SnapshotFork, "deposit", return_value={"status": "0x1"}) as deposit, \
                patch("builtins.print") as output:
            devnet.main()
        deposit.assert_called_once_with(7)
        self.assertEqual(json.loads(output.call_args.args[0]), {"status": "0x1"})

    def test_verify_command_uses_the_selected_fork_under_its_lock(self):
        fork = devnet.SnapshotFork(self.directory / "fork")
        fork.directory.mkdir()
        fork.manifest = manifest()
        fork.save()
        with patch.dict(os.environ, {"XDG_CONFIG_HOME": str(self.directory / "config")}):
            devnet.setup_path().parent.mkdir(parents=True)
            devnet.write_json(devnet.setup_path(), {"directory": str(fork.directory)})

            def locked_fork(*args):
                # The manifest is read only while this command holds the fork lock.
                with open(fork.directory / ".lock", "a") as other, self.assertRaises(BlockingIOError):
                    fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return devnet.SnapshotFork(*args)

            with patch.object(sys, "argv", ["verify"]), patch.object(verification, "verify") as verify, \
                    patch.object(verification, "SnapshotFork", side_effect=locked_fork):
                verification.main()
                self.assertEqual(verify.call_args.args[0].directory, fork.directory)
                self.assertEqual(verify.call_args.args[0].timeout, 7200)
                verify.reset_mock()
                with open(fork.directory / ".lock", "a") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    with self.assertRaises(BlockingIOError):
                        verification.main()
                verify.assert_not_called()
                empty = self.directory / "empty"
                empty.mkdir()
                with patch.object(sys, "argv", ["verify", "--dir", str(empty)]), \
                        self.assertRaisesRegex(RuntimeError, "missing manifest"):
                    verification.main()
                self.assertEqual(list(empty.iterdir()), [])

    @unittest.skipUnless(os.environ.get("BASE_SNAPSHOT_FORK_DIR"), "opt-in real snapshot fork qualification")
    def test_live_snapshot_fork(self):
        command = [sys.executable, str(Path(verification.__file__)), "--dir", os.environ["BASE_SNAPSHOT_FORK_DIR"]]
        subprocess.run(command, check=True)


if __name__ == "__main__":
    unittest.main()
