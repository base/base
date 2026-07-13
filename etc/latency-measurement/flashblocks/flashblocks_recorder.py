#!/usr/bin/env python3
"""Record Base Flashblocks websocket arrival times, to run ALONGSIDE `base-consensus observe`.

Purpose: decompose the CL gossip latency we already measure into (a) unavoidable geographic
network latency and (b) P2P/gossip-specific overhead.

  - Flashblocks arrive over a single websocket hop -> its us-east->APAC delta approximates the
    pure geographic/network floor that ANY protocol pays (and the floor a websocket fast path
    would hit).
  - CL gossip is multi-hop mesh -> its us-east->APAC delta is geographic + mesh overhead.

Run one of these next to each `base-consensus observe` node (same box / same NTP clock), then join
to the gossip observer CSV on block_number:

  P2P overhead      = gossip.recv_wallclock_ns - flashblock.recv_wallclock_ns   (same region, same block)
  geographic floor  = flashblock.recv[apac]    - flashblock.recv[us-east]        (same block)

CSV columns: recv_wallclock_ns,block_number,flashblock_index,region

Usage:
    pip install -r requirements.txt
    python flashblocks_recorder.py --url wss://<mainnet-flashblocks-ws> --region sydney \\
        --out /var/lib/base-observer/flashblocks-sydney.csv

NOTE: confirm the current Base mainnet flashblocks websocket URL (docs.base.org / the builder's
`--flashblocks` endpoint). Block-number extraction handles the FlashblocksPayloadV1 shape
(`metadata.block_number`, with `base.block_number` on index 0); sanity-check the first rows against
a live sample and adjust `extract_block_number` if the schema differs.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import time
from pathlib import Path

import websockets  # pip install websockets

HEADER = "recv_wallclock_ns,block_number,flashblock_index,region\n"


def extract_block_number(msg: dict) -> str:
    """Best-effort block number from a flashblocks message; '' if not found."""
    meta = msg.get("metadata") or {}
    if isinstance(meta, dict) and meta.get("block_number") is not None:
        return str(meta["block_number"])
    base = msg.get("base") or {}
    if isinstance(base, dict) and base.get("block_number") is not None:
        bn = base["block_number"]  # may be hex ("0x...") on index 0
        return str(int(bn, 16) if isinstance(bn, str) and bn.startswith("0x") else bn)
    return ""


async def run(url: str, region: str, out: Path) -> None:
    out.parent.mkdir(parents=True, exist_ok=True)
    new = not out.exists() or out.stat().st_size == 0
    with out.open("a", buffering=1) as fh:
        if new:
            fh.write(HEADER)
        async for ws in websockets.connect(url, max_size=None, ping_interval=20):
            try:
                async for raw in ws:
                    recv_ns = time.time_ns()  # wall-clock, NTP-synced; comparable across boxes
                    try:
                        msg = json.loads(raw)
                    except (ValueError, TypeError):
                        continue
                    fh.write(f"{recv_ns},{extract_block_number(msg)},{msg.get('index', '')},{region}\n")
            except websockets.ConnectionClosed:
                continue  # reconnect via the outer `async for`


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--url", required=True, help="flashblocks websocket URL (wss://...)")
    ap.add_argument("--region", required=True, help="region label (must match the observer's --p2p.latency.region)")
    ap.add_argument("--out", required=True, type=Path, help="output CSV path")
    args = ap.parse_args()
    print(f"recording flashblocks: url={args.url} region={args.region} -> {args.out}")
    asyncio.run(run(args.url, args.region, args.out))


if __name__ == "__main__":
    main()
