#!/usr/bin/env python3
"""Offline launcher tests."""

import copy
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
from unittest.mock import patch

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
                    self.assertIn("ambiguous", str(caught.exception))

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


if __name__ == "__main__":
    unittest.main()
