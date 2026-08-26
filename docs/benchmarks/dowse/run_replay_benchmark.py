#!/usr/bin/env python3
"""Run a compact Dowse/no-Dowse replay benchmark over a fixed canonical block range."""

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


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--rpc", default="http://127.0.0.1:18545")
parser.add_argument("--start-block", type=int, required=True)
parser.add_argument("--end-block", type=int, required=True)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--high-gas-threshold", type=int, default=200_000_000)
parser.add_argument("--raw-first", action="store_true")
args = parser.parse_args()

if args.end_block < args.start_block:
    parser.error("--end-block must not precede --start-block")

latest_at_start = int(rpc(args.rpc, "eth_blockNumber", []), 16)
if args.end_block > latest_at_start:
    parser.error(f"--end-block {args.end_block} exceeds current head {latest_at_start}")

args.output.parent.mkdir(parents=True, exist_ok=True)
with args.output.open("x", buffering=1) as output:
    metadata = {
        "kind": "metadata",
        "createdAt": datetime.now(timezone.utc).isoformat(),
        "latestAtStart": latest_at_start,
        "startBlock": args.start_block,
        "endBlock": args.end_block,
        "blockCount": args.end_block - args.start_block + 1,
        "highGasThreshold": args.high_gas_threshold,
        "initialCachedFirst": not args.raw_first,
    }
    output.write(json.dumps(metadata, separators=(",", ":")) + "\n")

    for index, block_number in enumerate(range(args.start_block, args.end_block + 1)):
        cached_first = (index % 2 == 0) != args.raw_first
        started = time.monotonic_ns()
        result = rpc(
            args.rpc,
            "base_benchmarkDowseBlockByNumber",
            [hex(block_number), cached_first],
        )
        wall_time_us = (time.monotonic_ns() - started) // 1_000

        raw_transactions = result["raw"]["transactions"]
        cached_transactions = result["cached"]["transactions"]
        raw_outcomes = [(tx["txHash"], tx["gasUsed"]) for tx in raw_transactions]
        cached_outcomes = [(tx["txHash"], tx["gasUsed"]) for tx in cached_transactions]
        if raw_outcomes != cached_outcomes:
            raise RuntimeError(f"replay outcome mismatch at block {block_number}")
        if result["raw"]["blockHash"] != result["cached"]["blockHash"]:
            raise RuntimeError(f"replay block hash mismatch at block {block_number}")

        prefetch = result.get("prefetch", result.get("prewarm"))
        record = {
            "kind": "block",
            "block": block_number,
            "gasUsed": sum(gas_used for _, gas_used in raw_outcomes),
            "transactionCount": len(raw_outcomes),
            "wallTimeUs": wall_time_us,
            "result": {
                "cachedFirst": cached_first,
                "prefetch": prefetch,
                "raw": compact_replay(result["raw"]),
                "cached": compact_replay(result["cached"]),
            },
        }
        output.write(json.dumps(record, separators=(",", ":")) + "\n")

        if (index + 1) % 25 == 0 or block_number == args.end_block:
            print(
                f'{index + 1}/{metadata["blockCount"]} blocks; '
                f'{block_number=}; gas={record["gasUsed"] / 1_000_000:.1f} Mgas',
                flush=True,
            )
