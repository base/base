#!/usr/bin/env python3
"""Offline launcher tests."""

import copy
import fcntl
import io
import json
import os
from pathlib import Path
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

import snapshot_devnet as devnet


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
    def test_rendered_inspection_compose_is_private_and_uses_immutable_images(self):
        config = json.loads(self.fork.compose("--profile", "inspect", "config", "--format", "json"))
        self.assertTrue(config["networks"]["private"]["internal"])
        self.assertEqual(set(config["services"]), {"inspect-sequencer", "inspect-validator"})
        for name, service in config["services"].items():
            self.assertTrue(service["image"].startswith("sha256:"))
            self.assertEqual(set(service["networks"]), {"private"})
            self.assertNotIn("ports", service)
            self.assertNotIn("restart", service)
            role = name.removeprefix("inspect-")
            self.assertIn({"source": self.fork.manifest["datadirs"][role], "target": "/data"},
                          [{key: volume[key] for key in ("source", "target")} for volume in service["volumes"]])
            self.assertIn("--disable-discovery", service["command"])

    def test_init_retry_preserves_identity_and_keys_and_completed_init_is_a_noop(self):
        config = self.preparation()
        config_path = self.root / "input.json"
        devnet.write_json(config_path, config)
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

    def test_boundary_requires_matching_snapshots_with_origins_before_fork(self):
        initial = snapshot()
        devnet.validate_boundary([initial, copy.deepcopy(initial)], 19)
        with self.assertRaisesRegex(RuntimeError, "origin is after the fork"):
            devnet.validate_boundary([initial, initial], 18)
        other = copy.deepcopy(initial)
        other["latest"]["system_config"]["batcherAddr"] = "0xother"
        with self.assertRaisesRegex(RuntimeError, "heads or system configs"):
            devnet.validate_boundary([initial, other], 19)

    def test_snapshot_l1_origins_must_be_canonical_upstream(self):
        initial = snapshot()
        with patch.object(devnet, "rpc", return_value={"hash": "0x19"}):
            devnet.validate_origins([initial], "https://rpc.invalid")
        for header in ({"hash": "0xother"}, None):
            with self.subTest(header=header), patch.object(devnet, "rpc", return_value=header):
                with self.assertRaisesRegex(RuntimeError, "noncanonical L1 origin"):
                    devnet.validate_origins([initial], "https://rpc.invalid")

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

    @unittest.skipUnless(shutil.which("just"), "requires the just command dispatcher")
    def test_snapshot_is_a_just_module_that_forwards_init_arguments_without_writes(self):
        for args in (["devnet", "snapshot"], ["--list", "devnet", "snapshot"]):
            with self.subTest(args=args):
                result = subprocess.run(["just", *args], cwd=devnet.ROOT, capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                for command in ("init", "test"):
                    self.assertIn(command, result.stdout)
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


if __name__ == "__main__":
    unittest.main()
