#!/usr/bin/env python3
"""Offline verifier tests for single-node forks and fast Denim scheduling."""

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import snapshot_devnet as devnet
import snapshot_verify as verification


SINGLE = ("sequencer",)
BOTH = ("sequencer", "validator")


def window():
    """The final pre-Denim block and the seven 200ms blocks after activation at 1000."""
    before = {"timestamp": hex(998), "hash": "before", "gasLimit": hex(40_000_030),
              "extraData": "0x010000003d00000006000000000000000b"}
    blocks, parent = [], "before"
    for index, milliseconds in enumerate((0, 200, 400, 600, 800, 0, 200)):
        blocks.append({"timestamp": hex(1000 + (index >= 5)), "timestampMs": hex(1_000_000 + 200 * index),
                       "parentHash": parent, "hash": str(index), "gasLimit": hex(4_000_003),
                       "extraData": "0x010000026200000006000000000000000b",
                       "transactions": [{"type": "0x7e"}, {"type": "0x7e", "to": verification.BASE_TIME,
                                         "input": "0x86bdf394" + f"{milliseconds:064x}"}, {"type": "0x2"}]})
        parent = str(index)
    return before, blocks


def headers():
    before, blocks = window()
    result = {124: before, **dict(enumerate(blocks, 125))}
    for height in (123, 132, 133, 134, 135, 136):
        result[height] = {"hash": str(height), "timestamp": hex(990 if height == 123 else 1002)}
    for height, header in result.items():
        header.update(number=hex(height), stateRoot=f"root-{height}")
    return result


def fork_fixture(directory, roles, **manifest):
    """A fork with only the explicitly configured roles."""
    fork = Mock(spec=devnet.SnapshotFork)
    fork.roles = roles
    fork.directory, fork.timeout = Path(directory), 1
    fork.manifest = {"phase": "running", "denim_timestamp": 1000, "epoch_slots": 3,
                     "accounts": {"user": "0x" + "d" * 40},
                     "initial": [{"latest": {"block_info": {"number": 123}},
                                  "rollup_config": {"genesis": {"l2_time": 0}, "block_time": 2}}], **manifest}
    return fork


def no_validator(test, role):
    test.assertNotIn("validator", str(role), "single-node verification must never contact a validator")


class VerifyModeTests(unittest.TestCase):
    def run_verify(self, roles, fast, transactions):
        """Runs verify with RPCs scripted; returns the report, the fork and the ordered lifecycle events."""
        blocks = headers()
        receipts = [{"blockNumber": hex(height), "blockHash": blocks[height]["hash"]} for height in transactions]
        events = []
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        fork = fork_fixture(directory.name, roles, **({"fast_denim": True} if fast else {}))

        def url(role):
            if roles == SINGLE:
                no_validator(self, role)
            return role

        def sync_status(role):
            if roles == SINGLE:
                no_validator(self, role)
            return {"unsafe_l2": {"l1origin": {"number": 10}}, "safe_l2": {"number": 122, "l1origin": {"number": 100}}}

        def rpc(endpoint, method, *args):
            if roles == SINGLE:
                no_validator(self, endpoint)
            if method == "admin_sequencerActive":
                return False
            if method == "eth_getBalance":
                return hex(10**18)
            if method == "eth_blockNumber":
                return hex(150 if endpoint == "l1" else 132)
            if method == "eth_getBlockByNumber":
                return {"timestamp": hex(900)} if args[0] == "latest" else blocks[int(args[0], 16)]
            self.fail(f"unexpected RPC: {method}")

        fork.url.side_effect = url
        fork.sync_status.side_effect = sync_status
        fork.stop.side_effect = lambda: events.append("stop")
        with patch.object(verification, "rpc", side_effect=rpc) as rpc_mock, \
                patch.object(verification, "transact", side_effect=receipts), \
                patch.object(verification, "derived", side_effect=lambda f, h: blocks[h]) as derived, \
                patch.object(verification, "local_blobs", return_value=[{"path": "blob"}]), \
                patch.object(verification, "wait_denim_window", side_effect=lambda *_: events.append("window")), \
                patch.object(verification, "assert_retained"), patch("builtins.print"):
            verification.verify(fork)
        report = json.loads((fork.directory / "verification.json").read_text())
        self.assertEqual(report["receipts"], receipts)
        self.assertEqual(report["activation_block_timestamp"], 1000)
        return report, fork, events, {c.args[1] for c in derived.call_args_list}, rpc_mock

    def test_single_node_runs_full_sequence_without_contacting_validator_or_claiming_parity(self):
        report, fork, events, derived, rpc = self.run_verify(SINGLE, False, (123, *range(124, 136)))
        self.assertEqual(report["roles"], ["sequencer"])
        self.assertTrue(report["independent_validator_check"].startswith("not run"))
        self.assertIn("single-node", report["safety_checks"])
        self.assertNotIn("passed", json.dumps(report))
        self.assertEqual(derived, {123, *range(124, 132), 133, 134, 135})
        self.assertEqual(events, ["stop", "window", "stop", "stop"])
        self.assertEqual([c.args for c in fork.compose.call_args_list], [
            ("stop", "batcher"), ("stop", "batcher"), ("kill", "--signal", "SIGKILL", "sequencer"), ("stop", "l1")])
        self.assertNotIn("admin_sequencerActive", [c.args[1] for c in rpc.call_args_list])
        fork.schedule_denim.assert_called_once_with()

    def test_two_node_retains_independent_validator_checks(self):
        report, fork, events, _, rpc = self.run_verify(BOTH, False, (123, *range(124, 136)))
        self.assertEqual(report["roles"], ["sequencer", "validator"])
        self.assertEqual(report["independent_validator_check"], "passed")
        self.assertIn(("validator-cl", "admin_sequencerActive"), [c.args[:2] for c in rpc.call_args_list])
        self.assertEqual({c.args[0] for c in fork.sync_status.call_args_list}, {"sequencer", "validator"})
        self.assertIn(("kill", "--signal", "SIGKILL", "sequencer", "validator"),
                      [c.args for c in fork.compose.call_args_list])
        self.assertEqual(events, ["stop", "window", "stop", "stop"])

    def test_fast_denim_reaches_transition_before_any_restart_and_still_checks_safety_after(self):
        for roles in (SINGLE, BOTH):
            with self.subTest(roles=roles):
                # Window transactions come first; the safety transaction (133) follows activation.
                report, fork, events, derived, _ = self.run_verify(roles, True, (*range(124, 133), 133, 134, 135, 136))
                self.assertEqual(events, ["window", "stop", "stop"])
                self.assertEqual(derived, {*range(124, 132), 133, 134, 135, 136})
                self.assertEqual(report["blobs"], [{"path": "blob"}])
                self.assertIn("mock", report["denim_schedule"])
                self.assertEqual(fork.start.call_count, 3)

    def test_normal_mode_reports_real_contract_schedule(self):
        report, *_ = self.run_verify(BOTH, False, (123, *range(124, 136)))
        self.assertNotIn("mock", report["denim_schedule"])

    def test_missed_transition_is_rejected_in_every_mode(self):
        for roles in (SINGLE, BOTH):
            for fast in (False, True):
                for latest in (998, 1005):
                    with self.subTest(roles=roles, fast=fast, latest=latest), tempfile.TemporaryDirectory() as directory:
                        fork = fork_fixture(directory, roles, **({"fast_denim": True} if fast else {}))
                        fork.url.side_effect = lambda role: role

                        def rpc(endpoint, method, *args):
                            if method == "admin_sequencerActive":
                                return False
                            return {"timestamp": hex(latest)}

                        with patch.object(verification, "rpc", side_effect=rpc), patch("builtins.print"):
                            with self.assertRaisesRegex(RuntimeError, "before Denim activation"):
                                verification.verify(fork)
                        fork.deposit.assert_not_called()
                        fork.stop.assert_not_called()
                        self.assertFalse((fork.directory / "verification.json").exists())

    def test_fast_mode_rejects_lead_consumed_before_the_window_without_downgrading(self):
        with tempfile.TemporaryDirectory() as directory:
            fork = fork_fixture(directory, SINGLE, fast_denim=True)
            fork.url.side_effect = lambda role: role
            fork.sync_status.return_value = {"unsafe_l2": {"l1origin": {"number": 10}}}
            latest = iter((900, 999))

            def rpc(endpoint, method, *args):
                if method == "eth_getBalance":
                    return hex(1)
                if method == "eth_blockNumber":
                    return hex(150)
                return {"number": "0x1", "timestamp": hex(next(latest))}

            with patch.object(verification, "rpc", side_effect=rpc), \
                    patch.object(verification, "transact") as transact, patch("builtins.print"):
                with self.assertRaisesRegex(RuntimeError, "too late"):
                    verification.verify(fork)
            transact.assert_not_called()
            fork.stop.assert_not_called()
            self.assertFalse((fork.directory / "verification.json").exists())


class SingleNodeHelperTests(unittest.TestCase):
    def test_single_node_derivation_waits_for_sequencer_safe_head(self):
        fork = fork_fixture("/unused", SINGLE)
        fork.url.side_effect = lambda role: (no_validator(self, role), role)[1]
        fork.sync_status.side_effect = lambda role: (no_validator(self, role), {"safe_l2": {"number": 124}})[1]
        header = {"number": "0x7c", "hash": "canonical", "stateRoot": "root"}
        with patch.object(verification, "rpc", return_value=header) as rpc, patch("builtins.print"):
            self.assertEqual(verification.derived(fork, 124), header)
        self.assertEqual([c.args[0] for c in rpc.call_args_list], ["sequencer"])

    def test_two_node_derivation_still_requires_validator_parity(self):
        fork = fork_fixture("/unused", BOTH)
        fork.url.side_effect = lambda role: role
        fork.sync_status.return_value = {"safe_l2": {"number": 124}}
        blocks = {"sequencer": {"number": "0x7c", "hash": "a", "stateRoot": "a"},
                  "validator": {"number": "0x7c", "hash": "a", "stateRoot": "b"}}
        with patch.object(verification, "rpc", side_effect=lambda endpoint, *_: blocks[endpoint]), \
                patch("builtins.print"), self.assertRaisesRegex(RuntimeError, "independently derived"):
            verification.derived(fork, 124)
        fork.sync_status.assert_called_with("validator")

    def test_single_node_retention_checks_only_the_sequencer(self):
        fork = fork_fixture("/unused", SINGLE)
        fork.url.side_effect = lambda role: (no_validator(self, role), role)[1]
        fork.sync_status.side_effect = lambda role: (no_validator(self, role), {"safe_l2": {"number": 124}})[1]
        saved = {"number": "0x7c", "hash": "a", "stateRoot": "a"}
        receipt = {"transactionHash": "0xt", "blockHash": "a", "status": "0x1"}
        report = {"blocks": [saved], "blobs": [], "receipts": [receipt]}
        with patch.object(verification, "rpc", side_effect=lambda endpoint, method, *_:
                          receipt if method == "eth_getTransactionReceipt" else saved) as rpc:
            verification.assert_retained(fork, report)
        self.assertEqual({c.args[0] for c in rpc.call_args_list}, {"sequencer"})
        with patch.object(verification, "rpc", return_value={**saved, "stateRoot": "b"}), \
                self.assertRaisesRegex(RuntimeError, "sequencer block hash/state root changed across restart"):
            verification.assert_retained(fork, report)


class RolesContractTests(unittest.TestCase):
    """The verifier uses the same roles as the launcher."""

    def test_roles_follow_manifest_datadirs(self):
        for datadirs, expected in (({"sequencer": "/s", "validator": "/v"}, BOTH), ({"sequencer": "/s"}, SINGLE)):
            with self.subTest(datadirs=datadirs), tempfile.TemporaryDirectory() as directory:
                (Path(directory) / "manifest.json").write_text(json.dumps({"datadirs": datadirs}))
                fork = devnet.SnapshotFork(directory)
                self.assertEqual(fork.roles, expected)


if __name__ == "__main__":
    unittest.main()
