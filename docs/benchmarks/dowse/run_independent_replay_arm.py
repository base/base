#!/usr/bin/env python3
"""Replay a fixed canonical block range once with one selected Dowse treatment."""

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
parser.add_argument("--variant", choices=("raw", "dowse"), required=True)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()

if args.end_block < args.start_block:
    parser.error("--end-block must not precede --start-block")

latest_at_start = int(rpc(args.rpc, "eth_blockNumber", []), 16)
if args.end_block > latest_at_start:
    parser.error(f"--end-block {args.end_block} exceeds current head {latest_at_start}")

dowse_cache_enabled = args.variant == "dowse"
args.output.parent.mkdir(parents=True, exist_ok=True)
with args.output.open("x", buffering=1) as output:
    metadata = {
        "kind": "metadata",
        "createdAt": datetime.now(timezone.utc).isoformat(),
        "latestAtStart": latest_at_start,
        "startBlock": args.start_block,
        "endBlock": args.end_block,
        "blockCount": args.end_block - args.start_block + 1,
        "variant": args.variant,
    }
    output.write(json.dumps(metadata, separators=(",", ":")) + "\n")

    for index, block_number in enumerate(range(args.start_block, args.end_block + 1)):
        started = time.monotonic_ns()
        result = rpc(
            args.rpc,
            "base_replayDowseBlockByNumber",
            [hex(block_number), dowse_cache_enabled],
        )
        wall_time_us = (time.monotonic_ns() - started) // 1_000

        if result["dowseCacheEnabled"] != dowse_cache_enabled:
            raise RuntimeError(f"unexpected treatment at block {block_number}")
        replay = result["replay"]
        if replay["blockNumber"] != block_number:
            raise RuntimeError(f"unexpected replay block at {block_number}")
        gas_used = sum(transaction["gasUsed"] for transaction in replay["transactions"])
        record = {
            "kind": "block",
            "block": block_number,
            "gasUsed": gas_used,
            "transactionCount": len(replay["transactions"]),
            "wallTimeUs": wall_time_us,
            "prefetch": result.get("prefetch"),
            "replay": compact_replay(replay),
        }
        output.write(json.dumps(record, separators=(",", ":")) + "\n")

        if (index + 1) % 25 == 0 or block_number == args.end_block:
            print(
                f'{index + 1}/{metadata["blockCount"]} blocks; '
                f'{block_number=}; gas={gas_used / 1_000_000:.1f} Mgas',
                flush=True,
            )
