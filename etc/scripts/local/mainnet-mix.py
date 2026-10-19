#!/usr/bin/env python3
"""Sample a Base block window and report its transaction mix.

    etc/scripts/local/mainnet-mix.py --cache /tmp/mix.jsonl --end <block>

Blocks land in --cache as they arrive; rerun with the same window to resume. To check a
load-test run, pass the test node's --rpc and the run's --start/--end with --samples at least
the range length.

Each non-deposit transaction takes the first class it matches, in CLASSES order:

    transfer        empty calldata
    swap            DEX swap event
    liquidity       Uniswap V3 pool or position event
    aa_bundle       ERC-4337 handleOps call or UserOperationEvent
    oracle          FeedUpdated event
    token_transfer  ERC-20 transfer or transferFrom call
    approval        only Approval events
    token_move      ERC-20 Transfer event (3 topics; ERC-721's 4-topic Transfer is `other`)
    reverted        failed
    no_logs         succeeded without logs
    other           anything else

It reports each class's share of transactions and gas, gas per transaction, the gap between the
even and odd halves of the sample as a precision estimate, and the top gas spenders in `other`.
Blocks show no storage writes and the public RPC rejects tracing, so ERC-20 Transfer events (up to
two balance writes each) stand in for them; approvals, pool state and other writes go uncounted.
"""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import json
import os
import sys
import time
import urllib.request

BLOCK_TIME = 2
BLOCKS_PER_BATCH = 2
DEPOSIT_TYPE = "0x7e"
TOKEN_SELECTORS = {"0xa9059cbb", "0x23b872dd"}
AA_SELECTORS = {"0x765e827f", "0x1fad948c"}  # EntryPoint v0.7 and v0.6 handleOps
USER_OPERATION_EVENT = "0x49628fd1471006c1482da88028e9ce4dbb080b815c9b0344d39e5a8e6ec1419f"
FEED_UPDATED = "0x29e088dffd7bf6815d088137a59f364d6747163baa7ff4bc47f316a1ad1b9586"
APPROVAL = "0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925"
ERC20_TRANSFER = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
SWAP_TOPICS = {
    "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822",  # Uniswap V2
    "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67",  # Uniswap V3, Slipstream
    "0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f",  # Uniswap V4
    "0xb3e2773606abfd36b5bd91394b3a54d1398336c65005baf7bf7a05efeffaf75b",  # Aerodrome/Velodrome V2
    "0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83",  # PancakeSwap V3
    "0x2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b",  # Balancer V2
    "0x8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140",  # Curve TokenExchange
}
LIQUIDITY_TOPICS = {
    "0x7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde",  # pool Mint
    "0x0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c",  # pool Burn
    "0x70935338e69775456a85ddef226c395fb668b63fa0115f5f20610b388e6ca9c0",  # pool Collect
    "0x3067048beee31b25b2f1681f88dac838c8bba36af25bfb2b7cf7473a5847e35f",  # IncreaseLiquidity
    "0x26f6a048ee9138f2c0ce266f322cb99228e8d619ae2bff30c67f8dcf9d2377b4",  # DecreaseLiquidity
    "0x40d0efd1a53d60ecbf40971b9daf7dc90178c3aadc7aab1765632738fa8b8f01",  # position Collect
}
CLASSES = ("transfer", "swap", "liquidity", "aa_bundle", "oracle", "token_transfer", "approval", "token_move",
           "reverted", "no_logs", "other")



def rpc_batch(url, calls, retries=8):
    payload = json.dumps([dict(jsonrpc="2.0", id=i, method=m, params=p) for i, (m, p) in enumerate(calls)])
    # Cloudflare in front of mainnet.base.org rejects urllib's default User-Agent.
    headers = {"Content-Type": "application/json", "User-Agent": "base-mainnet-mix"}
    for attempt in range(retries + 1):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, payload.encode(), headers), timeout=120) as r:
                body = json.load(r)
            # Rate limits return one error object, not a list; a block past a lagging backend's head is null.
            if isinstance(body, list) and all(item.get("result") is not None for item in body):
                return [item["result"] for item in sorted(body, key=lambda item: item["id"])]
        except (OSError, ValueError) as error:  # ValueError: a non-JSON body, e.g. a Cloudflare page
            body = error
        time.sleep(min(2**attempt, 30))
    raise RuntimeError(f"RPC batch failed after retries; last response: {str(body)[:300]}")


def classify(tx, receipt):
    topics = {log["topics"][0] for log in receipt["logs"] if log["topics"]}
    selector = tx["input"][:10]
    if tx["input"] == "0x" and tx["to"]:
        return "transfer"
    if topics & SWAP_TOPICS:
        return "swap"
    if topics & LIQUIDITY_TOPICS:
        return "liquidity"
    if selector in AA_SELECTORS or USER_OPERATION_EVENT in topics:
        return "aa_bundle"
    if FEED_UPDATED in topics:
        return "oracle"
    if selector in TOKEN_SELECTORS:
        return "token_transfer"
    if topics == {APPROVAL}:
        return "approval"
    if any(len(log["topics"]) == 3 and log["topics"][0] == ERC20_TRANSFER for log in receipt["logs"]):
        return "token_move"
    if receipt["status"] == "0x0":
        return "reverted"
    return "other" if topics else "no_logs"


def fetch(url, numbers):
    calls = [call for n in numbers for call in (("eth_getBlockByNumber", [hex(n), True]), ("eth_getBlockReceipts", [hex(n)]))]
    results = rpc_batch(url, calls)
    rows = []
    for n, block, receipts in zip(numbers, results[::2], results[1::2]):
        row = dict(n=n, time=int(block["timestamp"], 16), txs={}, gas={}, events=0, other_to={}, other_sel={})
        for tx, receipt in zip(block["transactions"], receipts):
            if tx["type"] == DEPOSIT_TYPE:
                continue
            kind, used = classify(tx, receipt), int(receipt["gasUsed"], 16)
            row["txs"][kind] = row["txs"].get(kind, 0) + 1
            row["gas"][kind] = row["gas"].get(kind, 0) + used
            row["events"] += sum(len(log["topics"]) == 3 and log["topics"][0] == ERC20_TRANSFER for log in receipt["logs"])
            if kind == "other":
                for field, key in (("other_to", tx["to"]), ("other_sel", tx["input"][:10])):
                    row[field][key] = row[field].get(key, 0) + used
        rows.append(row)
    return rows


def sample(url, blocks, cache, workers):
    done = {}
    if cache and os.path.exists(cache):
        with open(cache) as f:
            *lines, partial = f.read().split("\n")
        os.truncate(cache, os.path.getsize(cache) - len(partial.encode()))  # drop a row cut off mid-write
        done = {row["n"]: row for row in map(json.loads, filter(None, lines))}
    todo = [n for n in blocks if n not in done]
    batches = [todo[i:i + BLOCKS_PER_BATCH] for i in range(0, len(todo), BLOCKS_PER_BATCH)]
    with open(cache or os.devnull, "a") as out, ThreadPoolExecutor(workers) as pool:
        for rows in pool.map(lambda batch: fetch(url, batch), batches):
            for row in rows:
                done[row["n"]] = row
                out.write(json.dumps(row) + "\n")
            out.flush()
            print(f"{len(done)} blocks cached", file=sys.stderr)
    return [done[n] for n in blocks]


def summarize(rows):
    txs, gas = Counter(), Counter()
    for row in rows:
        txs.update(row["txs"])
        gas.update(row["gas"])
    total_txs, total_gas = sum(txs.values()), sum(gas.values())
    return dict(
        blocks=len(rows),
        per_block=dict(gas=round(total_gas / len(rows)), txs=round(total_txs / len(rows), 1),
                       transfer_events=round(sum(row["events"] for row in rows) / len(rows), 1)),
        classes={kind: dict(tx_share=round(txs[kind] / total_txs, 4), gas_share=round(gas[kind] / total_gas, 4),
                            gas_per_tx=round(gas[kind] / txs[kind]) if txs[kind] else 0) for kind in CLASSES},
    )


def report(blocks, rows):
    utc = lambda row: datetime.fromtimestamp(row["time"], timezone.utc).strftime("%Y-%m-%d %H:%M")  # noqa: E731
    stats = dict(start=blocks[0], end=blocks[-1], start_time=utc(rows[0]), end_time=utc(rows[-1]), **summarize(rows))
    even, odd = summarize(rows[0::2]), summarize(rows[1::2])
    stats["halves_diff"] = dict(
        gas_per_block_pct=round(100 * (even["per_block"]["gas"] / odd["per_block"]["gas"] - 1), 2),
        classes_pt={kind: [round(100 * (even["classes"][kind][key] - odd["classes"][kind][key]), 2)
                           for key in ("tx_share", "gas_share")] for kind in CLASSES},
    )
    for field in ("other_to", "other_sel"):
        total = sum((Counter(row[field]) for row in rows), Counter())
        stats[f"top_{field}_by_gas"] = total.most_common(10)
    return stats


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--rpc", default="https://mainnet.base.org")
    parser.add_argument("--days", type=float, default=7, help="window length ending at --end")
    parser.add_argument("--start", type=int, help="first block; overrides --days")
    parser.add_argument("--end", type=int, help="last block (default: latest)")
    parser.add_argument("--samples", type=int, default=10080, help="blocks sampled evenly across the window")
    parser.add_argument("--workers", type=int, default=4, help="concurrent batch requests")
    parser.add_argument("--cache", help="JSON-lines file of fetched blocks, reused on rerun")
    args = parser.parse_args()
    end = args.end if args.end is not None else int(rpc_batch(args.rpc, [("eth_blockNumber", [])])[0], 16)
    start = args.start if args.start is not None else end - int(args.days * 86400 / BLOCK_TIME) + 1
    count = max(2, min(args.samples, end - start + 1))
    blocks = [start + round(i * (end - start) / (count - 1)) for i in range(count)]
    stats = report(blocks, sample(args.rpc, blocks, args.cache, args.workers))
    print(json.dumps(stats, indent=2))


if __name__ == "__main__":
    main()
