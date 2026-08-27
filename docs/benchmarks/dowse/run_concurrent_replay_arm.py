#!/usr/bin/env python3
"""Replay canonical blocks while Dowse prefetch workers race block execution."""

import argparse
import json
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path


def rpc(url, method, params):
    for attempt in range(3):
        try:
            request = urllib.request.Request(
                url,
                data=json.dumps(
                    {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
                ).encode(),
                headers={"content-type": "application/json"},
            )
            with urllib.request.urlopen(request, timeout=300) as response:
                payload = json.load(response)
            break
        except OSError:
            if attempt == 2:
                raise
            time.sleep(2**attempt)
    if "error" in payload:
        raise RuntimeError(f'{method} failed: {payload["error"]}')
    return payload["result"]


def compact_replay(replay):
    return {
        "blockHash": replay["blockHash"],
        "blockNumber": replay["blockNumber"],
        "signerRecoveryTimeUs": replay["signerRecoveryTimeUs"],
        "executionTimeUs": replay["executionTimeUs"],
        "totalTimeUs": replay["totalTimeUs"],
        "stateProvider": replay["stateProvider"],
    }


def block_list(value):
    try:
        blocks = [int(part) for part in value.split(",")]
    except ValueError as error:
        raise argparse.ArgumentTypeError("blocks must be comma-separated integers") from error
    if not blocks or any(block < 0 for block in blocks):
        raise argparse.ArgumentTypeError("blocks must be nonnegative integers")
    if len(blocks) != len(set(blocks)):
        raise argparse.ArgumentTypeError("blocks must not contain duplicates")
    return blocks


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--rpc", default="http://127.0.0.1:18545")
parser.add_argument("--start-block", type=int)
parser.add_argument("--end-block", type=int)
parser.add_argument("--blocks", type=block_list)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--workers", type=int, required=True)
parser.add_argument("--head-start-us", type=int, required=True)
parser.add_argument("--max-accounts-per-transaction", type=int, required=True)
parser.add_argument("--max-storage-slots-per-transaction", type=int, required=True)
args = parser.parse_args()

has_range = args.start_block is not None or args.end_block is not None
if args.blocks is not None and has_range:
    parser.error("--blocks is mutually exclusive with --start-block/--end-block")
if args.blocks is None and (args.start_block is None or args.end_block is None):
    parser.error("provide either --blocks or both --start-block and --end-block")
if args.blocks is None:
    if args.start_block < 0 or args.end_block < args.start_block:
        parser.error("block range must be nonnegative and end at or after start")
    blocks = list(range(args.start_block, args.end_block + 1))
else:
    blocks = args.blocks
if args.workers <= 0:
    parser.error("--workers must be positive")
for name in (
    "head_start_us",
    "max_accounts_per_transaction",
    "max_storage_slots_per_transaction",
):
    if getattr(args, name) < 0:
        parser.error(f'--{name.replace("_", "-")} must not be negative')

latest_at_start = int(rpc(args.rpc, "eth_blockNumber", []), 16)
if max(blocks) > latest_at_start:
    parser.error(f"requested block exceeds current head {latest_at_start}")

config = {
    "workers": args.workers,
    "headStartUs": args.head_start_us,
    "maxAccountsPerTransaction": args.max_accounts_per_transaction,
    "maxStorageSlotsPerTransaction": args.max_storage_slots_per_transaction,
}
args.output.parent.mkdir(parents=True, exist_ok=True)
with args.output.open("x", buffering=1) as output:
    metadata = {
        "kind": "metadata",
        "createdAt": datetime.now(timezone.utc).isoformat(),
        "latestAtStart": latest_at_start,
        "blocks": blocks,
        "blockCount": len(blocks),
        "config": config,
    }
    output.write(json.dumps(metadata, separators=(",", ":")) + "\n")

    for index, block_number in enumerate(blocks):
        canonical = rpc(args.rpc, "eth_getBlockByNumber", [hex(block_number), False])
        if canonical is None:
            raise RuntimeError(f"canonical block {block_number} not found")
        started = time.monotonic_ns()
        result = rpc(
            args.rpc,
            "base_replayConcurrentDowseBlockByNumber",
            [hex(block_number), config],
        )
        wall_time_us = (time.monotonic_ns() - started) // 1_000

        if result["config"] != config:
            raise RuntimeError(f"unexpected replay config at block {block_number}")
        replay = result["replay"]
        if replay["blockNumber"] != block_number:
            raise RuntimeError(f"unexpected replay block at {block_number}")
        if replay["blockHash"] != canonical["hash"]:
            raise RuntimeError(f"canonical hash changed at block {block_number}")
        transactions = replay["transactions"]
        if [transaction["txHash"] for transaction in transactions] != canonical["transactions"]:
            raise RuntimeError(f"transaction sequence changed at block {block_number}")
        replay_gas_used = sum(transaction["gasUsed"] for transaction in transactions)
        canonical_gas_used = int(canonical["gasUsed"], 16)
        if replay_gas_used != canonical_gas_used:
            raise RuntimeError(
                f"replay gas mismatch at block {block_number}: "
                f"canonical={canonical_gas_used}, replay={replay_gas_used}"
            )
        record = {
            "kind": "block",
            "block": block_number,
            "gasUsed": canonical_gas_used,
            "transactionCount": len(transactions),
            "wallTimeUs": wall_time_us,
            "config": result["config"],
            "prefetch": result["prefetch"],
            "replay": compact_replay(replay),
        }
        output.write(json.dumps(record, separators=(",", ":")) + "\n")

        if (index + 1) % 25 == 0 or index + 1 == len(blocks):
            print(
                f"{index + 1}/{len(blocks)} blocks; "
                f"block_number={block_number}; gas={canonical_gas_used / 1_000_000:.1f} Mgas",
                flush=True,
            )
