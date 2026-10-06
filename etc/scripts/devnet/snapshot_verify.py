#!/usr/bin/env python3
"""Opt-in verification against an initialized, running snapshot fork; never opens source snapshots."""

import argparse
import fcntl
import hashlib
import json
import sys
import time

from snapshot_devnet import DEFAULT_TIMEOUT, SnapshotFork, configured_directory, number, require, request_json, rpc, run, wait, write_json


BASE_TIME = "0x4200000000000000000000000000000000000030"


def check_parity(sequencer, validator):
    require(sequencer["hash"] == validator["hash"] and sequencer["stateRoot"] == validator["stateRoot"],
            "independently derived block hash/state root mismatch")


def check_denim_window(before, blocks, timestamp):
    require(number(before["timestamp"]) == timestamp - 2, "missing final pre-Denim block")
    previous = before
    # Holocene/Jovian extra data starts with version byte, uint32 denominator, uint32 elasticity.
    denominator = int.from_bytes(bytes.fromhex(before["extraData"][2:])[1:5], "big")
    require(denominator > 0 and len(blocks) >= 7, "need the activation, siblings and next whole second")
    for index, block in enumerate(blocks):
        expected_ms = timestamp * 1000 + index * 200
        require(number(block["timestampMs"]) == expected_ms, "incorrect Denim millisecond cadence")
        require(number(block["timestamp"]) == expected_ms // 1000, "incorrect whole-second timestamp")
        require(block["parentHash"] == previous["hash"], "noncanonical activation window")
        require(number(block["gasLimit"]) == number(before["gasLimit"]) // 10,
                "Denim gas scaling must happen exactly once")
        extra = bytes.fromhex(block["extraData"][2:])
        require(int.from_bytes(extra[1:5], "big") == denominator * 10,
                "Denim denominator scaling must happen exactly once")
        transactions = block["transactions"]
        require(len(transactions) >= 2 and number(transactions[0]["type"]) == 126,
                "missing L1-info deposit at tx[0]")
        deposit = transactions[1]
        require(number(deposit["type"]) == 126 and deposit["to"].lower() == BASE_TIME,
                "missing BaseTime deposit at tx[1]")
        require(deposit["input"].lower() == "0x86bdf394" + f"{expected_ms % 1000:064x}",
                "BaseTime does not match the block timestamp")
        previous = block


def block(fork, role, height, full=False):
    result = rpc(fork.url(role), "eth_getBlockByNumber", hex(height), full)
    require(result is not None, f"missing {role} block {height}")
    return result


def derived(fork, height):
    wait("validator independently deriving safe block",
         lambda: fork.sync_status("validator")["safe_l2"]["number"] >= height, fork.timeout)
    sequencer = block(fork, "sequencer", height)
    check_parity(sequencer, block(fork, "validator", height))
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
            require(payload["data"] and all(payload["data"]), "local batch blobs unavailable")
            result.append({"path": path, "sha256": hashlib.sha256(json.dumps(payload, sort_keys=True).encode()).hexdigest(),
                           "l1_block": height, "transaction": tx["hash"]})
    require(result, "no locally posted blob batch found")
    return result


def assert_retained(fork, report):
    fork.assert_local_l1()
    fork.validate_restored_contracts()
    for saved in report["blocks"]:
        check_parity(saved, block(fork, "sequencer", number(saved["number"])))
        check_parity(saved, block(fork, "validator", number(saved["number"])))
    for saved in report["blobs"]:
        actual = request_json(fork.url("l1") + saved["path"])
        require(hashlib.sha256(json.dumps(actual, sort_keys=True).encode()).hexdigest() == saved["sha256"],
                "local blobs changed or disappeared across restart")
    for receipt in report["receipts"]:
        for role in ("sequencer", "validator"):
            actual = rpc(fork.url(role), "eth_getTransactionReceipt", receipt["transactionHash"])
            require(actual and actual["blockHash"] == receipt["blockHash"] and number(actual["status"]) == 1,
                    "transaction receipt lost across restart")


def denim_activation_time(config, timestamp):
    """Production Denim starts at the first legacy slot at or after the contract timestamp."""
    genesis, interval = number(config["genesis"]["l2_time"]), number(config["block_time"])
    return genesis + (max(0, timestamp - genesis) + interval - 1) // interval * interval


def wait_denim_window(fork, timestamp):
    head = {}

    def approaching():
        head.update(rpc(fork.url("sequencer"), "eth_getBlockByNumber", "latest", False))
        return number(head["timestamp"]) >= timestamp - 6

    wait("Denim transaction window (real contract notice period, no clock warp)", approaching, fork.timeout,
         progress=lambda: (head["number"], f"L2 timestamp {number(head['timestamp'])}; Denim at {timestamp}"))
    require(number(head["timestamp"]) < timestamp - 2,
            "verification started too late to exercise the final pre-Denim block")


def verify(fork):
    require(fork.manifest["phase"] == "running", "start the prepared fork first")
    fork.assert_local_l1()
    fork.validate_restored_contracts()
    require(not rpc(fork.url("validator-cl"), "admin_sequencerActive"),
            "validator must derive independently with sequencing stopped")
    fork.schedule_denim()
    timestamp = denim_activation_time(fork.manifest["initial"][0]["rollup_config"], fork.manifest["denim_timestamp"])
    require(number(rpc(fork.url("sequencer"), "eth_getBlockByNumber", "latest", False)["timestamp"]) < timestamp - 2,
            "full verification must start before Denim activation; the transaction window has passed, "
            "use a fresh fork to exercise the transition")
    print("Verifying deposits, safe derivation and local batch blobs.", flush=True)
    fork.deposit(10**18)
    user = fork.manifest["accounts"]["user"]
    wait("real L1 deposit reaching L2", lambda: number(rpc(fork.url("sequencer"), "eth_getBalance", user, "latest")) > 0, fork.timeout)
    # Keep normal gossip connected. Only L1 derivation can promote the transaction's block to safe.
    report = {"blocks": [], "receipts": [], "blobs": []}
    initial_origin = fork.sync_status("sequencer")["unsafe_l2"]["l1origin"]["number"]
    first_l1 = number(rpc(fork.url("l1"), "eth_blockNumber")) + 1
    receipt = transact(fork)
    report["receipts"].append(receipt)
    report["blocks"].append(derived(fork, number(receipt["blockNumber"])))
    last_l1 = number(rpc(fork.url("l1"), "eth_blockNumber"))
    report["blobs"] = local_blobs(fork, first_l1, last_l1)
    wait("L1 origins crossing two simulated epochs",
         lambda: fork.sync_status("validator")["safe_l2"]["l1origin"]["number"] >= initial_origin + 2 * fork.manifest["epoch_slots"],
         fork.timeout)
    print("Verifying clean restart before Denim.", flush=True)
    fork.stop()
    fork.start()
    assert_retained(fork, report)

    print("Verifying transactions across Denim activation.", flush=True)
    wait_denim_window(fork, timestamp)
    deadline = time.monotonic() + fork.timeout
    while True:
        require(time.monotonic() < deadline, "Denim transaction window timed out")
        receipt = transact(fork)
        report["receipts"].append(receipt)
        if number(block(fork, "sequencer", number(receipt["blockNumber"]))["timestamp"]) >= timestamp + 2:
            break
    low = fork.manifest["initial"][0]["latest"]["block_info"]["number"]
    high = number(rpc(fork.url("sequencer"), "eth_blockNumber"))
    while low < high:
        middle = (low + high) // 2
        if number(block(fork, "sequencer", middle)["timestamp"]) < timestamp:
            low = middle + 1
        else:
            high = middle
    activation = low
    before = block(fork, "sequencer", activation - 1, True)
    window = [block(fork, "sequencer", activation + index, True) for index in range(7)]
    check_denim_window(before, window, timestamp)
    exercised = {receipt["blockHash"] for receipt in report["receipts"]}
    for header in [before, *window]:
        require(header["hash"] in exercised,
                "transaction generator missed an activation-window block; qualification incomplete")
        report["blocks"].append(derived(fork, number(header["number"])))
    receipt = transact(fork)
    report["receipts"].append(receipt)
    report["blocks"].append(derived(fork, number(receipt["blockNumber"])))
    report["denim_timestamp"] = fork.manifest["denim_timestamp"]
    report["activation_block_timestamp"] = timestamp
    report["blobs"] = local_blobs(fork, first_l1, number(rpc(fork.url("l1"), "eth_blockNumber")))
    print("Verifying clean restart after Denim.", flush=True)
    fork.stop()
    fork.start()
    assert_retained(fork, report)

    for interrupt in (False, True):
        print("Verifying interrupted recovery." if interrupt else "Verifying clean recovery of an unbatched transaction.",
              flush=True)
        # Leave a transaction included in unsafe L2 but not yet batched. Resume must
        # reconstruct batching from the safe head rather than depend on a process queue.
        fork.compose("stop", "batcher")
        receipt = transact(fork)
        require(fork.sync_status("validator")["safe_l2"]["number"] < number(receipt["blockNumber"]),
                "pending-batch fixture unexpectedly became safe")
        if interrupt:
            # Only the dedicated project's containers; no host process or other devnet.
            fork.compose("kill", "--signal", "SIGKILL", "sequencer", "validator")
            fork.compose("stop", "l1")
        else:
            fork.stop()
        fork.start()
        report["receipts"].append(receipt)
        report["blocks"].append(derived(fork, number(receipt["blockNumber"])))
        assert_retained(fork, report)
    write_json(fork.directory / "verification.json", report)
    print("Verified safe derivation, local blobs, Denim activation, clean restarts and interrupted recovery.")


def main():
    parser = argparse.ArgumentParser(description="Verify the full snapshot devnet: sends transactions, "
                                     "checks Denim activation, restarts services and kills L2 containers to test recovery.")
    parser.add_argument("--dir", help="override the fork directory selected by setup")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT)
    args = parser.parse_args()
    args.dir = configured_directory(args.dir)
    fork = SnapshotFork(args.dir, args.timeout)
    require(fork.manifest is not None, "missing manifest")
    require(args.timeout > 0, "timeout must be positive")
    with open(fork.directory / ".lock", "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fork = SnapshotFork(args.dir, args.timeout)
        verify(fork)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyError) as error:
        print(f"snapshot verification: {error}", file=sys.stderr)
        sys.exit(1)
