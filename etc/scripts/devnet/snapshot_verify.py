#!/usr/bin/env python3
"""Opt-in verification against an initialized, running snapshot fork; never opens source snapshots."""

import argparse
import fcntl
import hashlib
import json
import sys

from snapshot_devnet import DEFAULT_TIMEOUT, SnapshotFork, configured_directory, number, require, request_json, rpc, run, wait, write_json


def check_parity(sequencer, validator):
    require(sequencer["hash"] == validator["hash"] and sequencer["stateRoot"] == validator["stateRoot"],
            "independently derived block hash/state root mismatch")


def block(fork, role, height, full=False):
    result = rpc(fork.url(role), "eth_getBlockByNumber", hex(height), full)
    require(result is not None, f"missing {role} block {height}")
    return result


def derived(fork, receipt):
    """The receipt's block once the validator derives it as safe; agreement at that height alone
    could be a replacement block that orphaned the receipt."""
    height = number(receipt["blockNumber"])
    wait("validator independently deriving safe block",
         lambda: fork.sync_status("validator")["safe_l2"]["number"] >= height, fork.timeout)
    sequencer = block(fork, "sequencer", height)
    check_parity(sequencer, block(fork, "validator", height))
    require(sequencer["hash"] == receipt["blockHash"],
            f"safe block {height} replaced the transaction's unsafe block; its receipt was orphaned")
    actual = rpc(fork.url("validator"), "eth_getTransactionReceipt", receipt["transactionHash"])
    require(actual and actual["blockHash"] == receipt["blockHash"] and number(actual["status"]) == 1,
            f"validator lacks the transaction's successful receipt in safe block {height}")
    return {key: sequencer[key] for key in ("number", "hash", "stateRoot")}


def transact(fork):
    key = json.loads((fork.directory / "keys.json").read_text())["user"]
    transaction = run("cast", "send", "--rpc-url", fork.url("sequencer"), "--private-key", key,
                      "--chain-id", "8453", "--gas-limit", "21000", "--value", "1", "--async",
                      "0x000000000000000000000000000000000000bEEF")
    receipt = wait("sequencer transaction", lambda: rpc(fork.url("sequencer"), "eth_getTransactionReceipt", transaction),
                   fork.timeout, poll_interval=0.02)
    require(number(receipt["status"]) == 1, "local transaction reverted")
    return receipt


def local_blobs(fork, first, last):
    result = []
    for height in range(first, last + 1):
        header = block(fork, "l1", height, True)
        for tx in header["transactions"]:
            if tx["from"].lower() != fork.manifest["accounts"]["batcher"].lower():
                continue
            hashes = tx.get("blobVersionedHashes", [])
            if not hashes:
                continue
            timestamp = number(header["timestamp"])
            slot = (timestamp - number(fork.manifest["beacon_genesis"]["genesis_time"])) // fork.manifest["slot_seconds"]
            path = f"/eth/v1/beacon/blobs/{slot}?versioned_hashes=" + ",".join(hashes)
            payload = request_json(fork.url("l1") + path)
            # The beacon filter omits unknown hashes rather than failing; require one blob per hash.
            require(len(payload["data"]) == len(hashes) and all(payload["data"]), "local batch blobs unavailable")
            result.append({"path": path, "sha256": hashlib.sha256(json.dumps(payload, sort_keys=True).encode()).hexdigest(),
                           "l1_block": height, "transaction": tx["hash"]})
    require(result, "no locally posted blob batch found")
    return result


def verify(fork):
    require(fork.manifest["phase"] == "running", "start the prepared fork first")
    fork.require_batcher()
    fork.assert_local_l1()
    fork.validate_restored_contracts()
    require(not rpc(fork.url("validator-cl"), "admin_sequencerActive"),
            "validator must derive independently with sequencing stopped")
    print("Verifying deposits, safe derivation and local batch blobs.", flush=True)
    # Reconcile a deposit the user already journaled, such as `deposit --wei N`, rather than conflict with it.
    recorded = fork.manifest["operations"].get("fund-user")
    fork.deposit(number(recorded["transaction"]["value"]) if recorded else 10**18)
    user = fork.manifest["accounts"]["user"]
    wait("real L1 deposit reaching L2", lambda: number(rpc(fork.url("sequencer"), "eth_getBalance", user, "latest")) > 0, fork.timeout)
    # Keep normal gossip connected. Only L1 derivation can promote the transaction's block to safe.
    report = {"blocks": [], "receipts": [], "blobs": []}
    first_l1 = number(rpc(fork.url("l1"), "eth_blockNumber")) + 1
    receipt = transact(fork)
    report["receipts"].append(receipt)
    report["blocks"].append(derived(fork, receipt))
    report["blobs"] = local_blobs(fork, first_l1, number(rpc(fork.url("l1"), "eth_blockNumber")))
    write_json(fork.directory / "verification.json", report)
    print("Verified the L1 deposit, safe derivation with matching block hash and state root, and local batch blobs.")


def main():
    parser = argparse.ArgumentParser(description="Verify a running snapshot fork: deposits through L1, sends a "
                                     "transaction, and checks its safe derivation and local batch blobs.")
    parser.add_argument("--dir", help="override the fork directory selected by setup")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT)
    args = parser.parse_args()
    directory = configured_directory(args.dir)
    require((directory / "manifest.json").is_file(), "missing manifest")
    require(args.timeout > 0, "timeout must be positive")
    with open(directory / ".lock", "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        verify(SnapshotFork(directory, args.timeout))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyError) as error:
        print(f"snapshot verification: {error}", file=sys.stderr)
        sys.exit(1)
