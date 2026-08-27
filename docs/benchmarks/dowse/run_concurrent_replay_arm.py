#!/usr/bin/env python3
"""Replay selected canonical blocks with raw state or concurrent Dowse prefetching."""

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
parser.add_argument("--variant", choices=("raw", "concurrent"), default="concurrent")
parser.add_argument("--workers", type=int, default=4)
parser.add_argument("--head-start-us", type=int, default=0)
parser.add_argument("--max-accounts-per-transaction", type=int, default=32)
parser.add_argument("--max-storage-slots-per-transaction", type=int, default=256)
parser.add_argument("--max-transaction-distance", type=int, default=4)
parser.add_argument("--locality-batch-size", type=int, default=1)
parser.add_argument("--min-confidence-bps", type=int, default=2000)
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
if args.locality_batch_size <= 0:
    parser.error("--locality-batch-size must be positive")
if not 0 <= args.min_confidence_bps <= 10000:
    parser.error("--min-confidence-bps must be between 0 and 10000")
for name in (
    "head_start_us",
    "max_accounts_per_transaction",
    "max_storage_slots_per_transaction",
    "max_transaction_distance",
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
    "maxTransactionDistance": args.max_transaction_distance,
    "localityBatchSize": args.locality_batch_size,
    "minConfidenceBps": args.min_confidence_bps,
}
args.output.parent.mkdir(parents=True, exist_ok=True)
with args.output.open("x", buffering=1) as output:
    metadata = {
        "kind": "metadata",
        "createdAt": datetime.now(timezone.utc).isoformat(),
        "latestAtStart": latest_at_start,
        "blocks": blocks,
        "blockCount": len(blocks),
        "variant": args.variant,
        "config": config if args.variant == "concurrent" else None,
    }
    output.write(json.dumps(metadata, separators=(",", ":")) + "\n")

    for index, block_number in enumerate(blocks):
        canonical = rpc(args.rpc, "eth_getBlockByNumber", [hex(block_number), False])
        if canonical is None:
            raise RuntimeError(f"canonical block {block_number} not found")
        started = time.monotonic_ns()
        if args.variant == "concurrent":
            result = rpc(
                args.rpc,
                "base_replayConcurrentDowseBlockByNumber",
                [hex(block_number), config],
            )
        else:
            result = rpc(
                args.rpc,
                "base_replayDowseBlockByNumber",
                [hex(block_number), False],
            )
        wall_time_us = (time.monotonic_ns() - started) // 1_000

        if args.variant == "concurrent" and result["config"] != config:
            raise RuntimeError(f"unexpected replay config at block {block_number}")
        if args.variant == "raw" and result["dowseCacheEnabled"]:
            raise RuntimeError(f"unexpected replay treatment at block {block_number}")
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
            "config": result.get("config"),
            "prefetch": result.get("prefetch"),
            "replay": compact_replay(replay),
        }
        output.write(json.dumps(record, separators=(",", ":")) + "\n")

        if (index + 1) % 25 == 0 or index + 1 == len(blocks):
            print(
                f"{index + 1}/{len(blocks)} blocks; "
                f"block_number={block_number}; gas={canonical_gas_used / 1_000_000:.1f} Mgas",
                flush=True,
            )
