#!/usr/bin/env python3
"""Sample a Base block window, report its transaction mix and write a load-test config for it.

    etc/scripts/local/mainnet-mix.py --cache /tmp/mix.jsonl --end <block> \
        --out crates/infra/load-tests/examples/mainnet-mix-mainnet-snapshot.yaml

Blocks land in --cache as they arrive; rerun with the same window to resume. To check a
load-test run, pass the test node's --rpc and the run's --start/--end with --samples at least
the range length.

Each non-deposit transaction takes the first class it matches, in CLASSES order:
x402 uses USDC AuthorizationUsed transactions as a proxy; other EIP-3009 users are included.


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

--out also writes <name>.targets.json beside the config: the sample's gas per tx, share of txs and gas
at >= 400k gas, calldata and logs per tx, and compressed DA bytes per gas (receipt l1GasUsed / 16,
Fjord's FastLZ size estimate), each with a relative tolerance. New accounts and storage slots per gas
need state diffs and stay null. `base-bench snapshot` compares a run against this file.

--out maps each class to gas-matched stand-ins (CLASS_MIX), weights them by the class's share of
transactions (1000 in total) and sets target_gps from the window's gas per second. A class no single
stand-in can match is split across several, so its share of txs and its mean gas both hold. It also
predicts the mix's gas, heavy-tx shares, calldata and logs per tx from per-stand-in receipts on a
Base mainnet fork (STAND_INS), and writes the prediction beside the targets in the config header.
Reverts run as successful swaps of similar gas: the generator has no failing-call type.
"""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
import json
import os
import sys
import time
import urllib.request

BLOCK_TIME = 2
BLOCKS_PER_BATCH = 2
DEPOSIT_TYPE = "0x7e"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
AUTHORIZATION_USED = "0x98de503528ee59b575ef0c0a2576a82497bfc029a5685b209e9ec333479b10a5"
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
CLASSES = ("x402", "transfer", "swap", "liquidity", "aa_bundle", "oracle", "token_transfer", "approval", "token_move",
           "reverted", "no_logs", "other")


WETH, USDC_CHECKSUM = "0x4200000000000000000000000000000000000006", "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
UNISWAP = (f'type: uniswap_v3, router: "0x2626664c2603336E57B271c5C0b26F421741e481", token_in: "{WETH}",\n'
           f'      token_out: "{USDC_CHECKSUM}", min_amount: "10000000000000", max_amount: "100000000000000",'
           ' reverse_amounts: auto,\n      ')
VIA_CBBTC = 'via: [{token: "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf", fee: 500}], fee: 500'  # cbBTC, gitleaks:allow
VIA_DAI_USDBC = ('via: [{token: "0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb", fee: 500},'  # DAI, gitleaks:allow
                 ' {token: "0xd9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA", fee: 100}], fee: 100')  # USDbC, gitleaks:allow
LIQUIDITY = (f'type: uniswap_v3_liquidity, position_manager: "0x03a520b32C04BF3bEEf7BEb72E919cf822Ed34f1",\n'
             f'      token0: "{WETH}", token1: "{USDC_CHECKSUM}", fee: 500, amount0: "10000000000000", amount1: "23000",\n      ')


def storage(slots, new_slot_ratio):
    return f"type: storage, slots_per_tx: {slots}, new_slot_ratio: {new_slot_ratio}"


# Stand-in: YAML fields, then mean receipt gas, calldata bytes and logs per tx from a 30-sender run on a Base
# mainnet anvil fork (block 53294591), and fresh storage slots per tx.
# Liquidity ops share one uniswap_v3_liquidity entry.
STAND_INS = dict(
    transfer=("type: transfer", 21_000, 0, 0, 0),
    erc20=(f'type: erc20, contract: "{USDC_CHECKSUM}"', 45_044, 68, 1, 0),
    x402=("type: x402", 85_756, 292, 2, 0),
    token_move_1=("type: token_move, transfers_per_tx: 1", 57_430, 228, 1, 0),
    token_move_10=("type: token_move, transfers_per_tx: 10", 161_054, 804, 10, 0),
    token_move_60=("type: token_move, transfers_per_tx: 60", 625_416, 4_004, 60, 0),
    swap_1_pool=(UNISWAP + "fee: 500", 119_486, 228, 3, 0),
    swap_2_pools=(UNISWAP + VIA_CBBTC, 204_166, 292, 6, 0),
    swap_3_pools=(UNISWAP + VIA_DAI_USDBC, 278_416, 292, 9, 0),
    liquidity_mint=(LIQUIDITY + "ops: {mint: 1}", 378_076, 356, 5, 0),
    liquidity_multicall=(LIQUIDITY + "ops: {multicall: 1}", 534_608, 1_284, 16, 0),
    aa_bundle_6=("type: aa_bundle, ops_per_bundle: 6", 242_173, 1_284, 6, 6),
    aa_bundle_16=("type: aa_bundle, ops_per_bundle: 16", 599_855, 3_204, 16, 16),
    storage_no_logs=(storage(5, 0.05), 57_262, 100, 0, 0.25),
    storage_oracle=(storage(8, 0.05), 75_616, 100, 0, 0.4),
    storage_other=(storage(16, 0.05), 125_769, 100, 0, 0.8),
    storage_heavy=(storage(215, 0.05), 1_177_272, 100, 0, 10.75),
)
# Stand-ins per class with their share of the class's txs. Splits keep each class's mean gas and add the
# >= 400k-gas tail; swaps and liquidity outweigh any one DEX stand-in.
CLASS_MIX = dict(
    transfer=dict(transfer=1), x402=dict(x402=1),
    swap=dict(swap_2_pools=0.38, swap_3_pools=0.48, storage_heavy=0.08, token_move_60=0.06),
    liquidity=dict(liquidity_mint=0.25, liquidity_multicall=0.3, storage_heavy=0.45),
    aa_bundle=dict(aa_bundle_6=0.58, aa_bundle_16=0.42), oracle=dict(storage_oracle=1),
    token_transfer=dict(token_move_1=1), approval=dict(erc20=1), token_move=dict(token_move_10=0.66, token_move_60=0.34),
    reverted=dict(swap_1_pool=0.6, swap_2_pools=0.4), no_logs=dict(storage_no_logs=1), other=dict(storage_other=1),
)
WEIGHT_SCALE = 1000
CLASSIFIER_VERSION = 3  # Caches before USDC authorization classification or per-tx targets cannot be reused.
HEAVY_GAS = 400_000
# Relative tolerances; the tail shares share gas per tx's.
TOLERANCES = dict(gas_per_tx=0.10, heavy_tx_share=0.10, heavy_gas_share=0.10, calldata_bytes_per_tx=0.25,
                  logs_per_tx=0.25, da_bytes_per_gas=0.15, new_accounts_per_gas=0.25, new_slots_per_gas=0.25)

CONFIG_TEMPLATE = """\
# Base mainnet transaction mix, generated by etc/scripts/local/mainnet-mix.py (mapping and limits).
# Measurement window: blocks {start}..{end} ({start_time} to {end_time} UTC / {start_est} to {end_est} EST), {blocks} samples.
# Non-deposit mean per block: {gas} gas, {txs} txs, {transfer_events} ERC-20 Transfer events.
# Class: share by count / gas, gas/tx -> stand-ins (share of class txs), predicted gas/tx:
{class_lines}
# Weights: class tx share x stand-in share, normalized to 1000. Predicted from stand-in fork receipts, vs this window:
{prediction_lines}
# State growth has no target (blocks carry no state diffs): stand-ins write {fresh_slots_per_mgas} fresh slots per Mgas
# (storage and aa_bundle recipients; liquidity mints uncounted); production monitoring shows ~0.00077 bytes/gas.
# Reverts run as successful swaps of similar gas; ERC-4337 bundles run the aa_bundle stand-in.
# For `base-bench snapshot`: the launcher replaces the RPC URLs and prefunds an ephemeral funder.
# Includes USDC EIP-3009 as an x402 proxy (not all authorizations are x402).
# Changed mix: benchmark runs using this YAML are not comparable with earlier mixes.

transaction_submission_rpcs: ["http://127.0.0.1:7545"]
query_rpc: "http://127.0.0.1:7545"
flashblocks_ws: "ws://127.0.0.1:7111"
chain_id: 8453
sender_count: 100
target_gps: {target_gps}
block_time: "2s"
duration: null
measurement_blocks: 500
skip_drain: true
funding_amount: "200000000000000000" # 0.2 ETH covers WETH setup plus swap gas

real_token_setup:
  enabled: true
  allow_chain_id_8453: true
  weth: "0x4200000000000000000000000000000000000006"
  weth_amount_per_sender: "50000000000000000" # 0.05 WETH
  pair_token:
    token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" # USDC, public token address: gitleaks:allow
    amount_per_sender: "10000000" # 10 USDC
    acquisition: {{type: uniswap_v3_exact_input, router: "0x2626664c2603336E57B271c5C0b26F421741e481",
      fee: 500, amount_in: "10000000000000000", min_amount_out: "0"}}
# Swap amounts: WETH wei forward; reverse_amounts: auto prices USDC reverse swaps at the pools.
transactions:
{transactions}
"""


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
    # USDC EIP-3009 is an upper-bound proxy for x402, not facilitator attribution.
    if any(log.get("address", "").lower() == USDC and log["topics"]
           and log["topics"][0] == AUTHORIZATION_USED for log in receipt["logs"]):
        return "x402"
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
        row = dict(n=n, time=int(block["timestamp"], 16), txs={}, gas={}, events=0, other_to={}, other_sel={},
                   heavy_txs=0, heavy_gas=0, calldata=0, logs=0, l1_gas=0)
        row["classifier_version"] = CLASSIFIER_VERSION
        for tx, receipt in zip(block["transactions"], receipts):
            if tx["type"] == DEPOSIT_TYPE:
                continue
            kind, used = classify(tx, receipt), int(receipt["gasUsed"], 16)
            row["txs"][kind] = row["txs"].get(kind, 0) + 1
            row["gas"][kind] = row["gas"].get(kind, 0) + used
            if used >= HEAVY_GAS:
                row["heavy_txs"] += 1
                row["heavy_gas"] += used
            row["calldata"] += len(tx["input"]) // 2 - 1
            row["logs"] += len(receipt["logs"])
            row["l1_gas"] += int(receipt["l1GasUsed"], 16)
            row["events"] += sum(len(log["topics"]) == 3 and log["topics"][0] == ERC20_TRANSFER for log in receipt["logs"])
            if kind == "other":
                for field, key in (("other_to", tx["to"]), ("other_sel", tx["input"][:10])):
                    row[field][key] = row[field].get(key, 0) + used
        rows.append(row)
    return rows


def validate_cache_rows(rows):
    """Reject cached aggregates computed by a different transaction classifier."""
    if any(row.get("classifier_version") != CLASSIFIER_VERSION for row in rows):
        raise ValueError("cache classifier version mismatch; collect a new cache file (old aggregates cannot be reclassified)")


def sample(url, blocks, cache, workers):
    done = {}
    if cache and os.path.exists(cache):
        with open(cache) as f:
            *lines, partial = f.read().split("\n")
        rows = list(map(json.loads, filter(None, lines)))
        validate_cache_rows(rows)
        os.truncate(cache, os.path.getsize(cache) - len(partial.encode()))  # drop a row cut off mid-write
        done = {row["n"]: row for row in rows}
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


def targets(stats, rows):
    txs, gas = sum(sum(row["txs"].values()) for row in rows), sum(sum(row["gas"].values()) for row in rows)
    total = lambda field: sum(row[field] for row in rows)  # noqa: E731
    measured = dict(gas_per_tx=gas / txs, heavy_tx_share=total("heavy_txs") / txs, heavy_gas_share=total("heavy_gas") / gas,
                    calldata_bytes_per_tx=total("calldata") / txs, logs_per_tx=total("logs") / txs,
                    da_bytes_per_gas=total("l1_gas") / 16 / gas, new_accounts_per_gas=None, new_slots_per_gas=None)
    return dict(start=stats["start"], end=stats["end"], blocks=stats["blocks"], metrics={
        name: dict(target=None if value is None else float(f"{value:.4g}"), tolerance=TOLERANCES[name])
        for name, value in measured.items()})


def report(blocks, rows):
    utc = lambda row: datetime.fromtimestamp(row["time"], timezone.utc).strftime("%Y-%m-%d %H:%M")  # noqa: E731
    est = lambda row: datetime.fromtimestamp(row["time"], timezone(timedelta(hours=-5))).strftime("%Y-%m-%d %H:%M")  # noqa: E731
    stats = dict(start=blocks[0], end=blocks[-1], start_time=utc(rows[0]), end_time=utc(rows[-1]),
                 start_est=est(rows[0]), end_est=est(rows[-1]), **summarize(rows))
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


def weights(classes):
    """Weight each stand-in by its classes' tx shares, normalized to exactly WEIGHT_SCALE."""
    total = sum(c["tx_share"] for c in classes.values())
    scaled = Counter()
    for kind, mix in CLASS_MIX.items():
        for name, share in mix.items():
            scaled[name] += classes[kind]["tx_share"] / total * share * WEIGHT_SCALE
    weight = {name: int(value) for name, value in scaled.items()}
    for name in sorted(weight, key=lambda n: scaled[n] % 1, reverse=True)[:WEIGHT_SCALE - sum(weight.values())]:
        weight[name] += 1
    return weight


def predict(weight):
    """Per-tx metrics of a mix from its stand-ins' fork receipts, keyed like targets()."""
    txs = sum(weight.values())
    mean = lambda f: sum(w * f(*STAND_INS[n][1:]) for n, w in weight.items()) / txs  # noqa: E731
    gas = mean(lambda g, *_: g)
    return dict(gas_per_tx=gas, heavy_tx_share=mean(lambda g, *_: g >= HEAVY_GAS),
                heavy_gas_share=mean(lambda g, *_: g * (g >= HEAVY_GAS)) / gas,
                calldata_bytes_per_tx=mean(lambda g, c, *_: c), logs_per_tx=mean(lambda g, c, logs, _: logs),
                fresh_slots_per_mgas=mean(lambda *s: s[3]) / gas * 1e6)


def render(stats, targets):
    classes = stats["classes"]
    weight = weights(classes)
    predicted = predict(weight)
    class_lines = "\n".join(
        f"#   {kind:<15}{c['tx_share']:7.2%} / {c['gas_share']:7.2%}, {c['gas_per_tx']:>7} -> "
        + " + ".join(f"{name} {share:.0%}" for name, share in CLASS_MIX[kind].items())
        + f", {round(sum(share * STAND_INS[name][1] for name, share in CLASS_MIX[kind].items()))}"
        for kind, c in classes.items()
    )
    prediction_lines = "\n".join(
        f"#   {name:<22}{predicted[name]:>10.4g} vs {metric['target']:.4g} ({predicted[name] / metric['target'] - 1:+.1%},"
        f" tolerance {metric['tolerance']:.0%})"
        for name, metric in targets["metrics"].items() if name in predicted and metric["target"]
    )
    transactions = []
    liquidity = {name.removeprefix("liquidity_"): weight.pop(name) for name in list(weight) if name.startswith("liquidity_")}
    for name, w in weight.items():
        transactions.append(f"  - {{weight: {w}, {STAND_INS[name][0]}}}")
    if sum(liquidity.values()):
        ops = ", ".join(f"{op}: {w}" for op, w in liquidity.items())
        fields = STAND_INS["liquidity_mint"][0].replace("ops: {mint: 1}", f"ops: {{{ops}}}")
        transactions.append(f"  - {{weight: {sum(liquidity.values())}, {fields}}}")
    # Public token and contract addresses, not secrets.
    transactions = "\n".join(
        "\n".join(f"{line} # gitleaks:allow" if '"0x' in line else line for line in entry.split("\n"))
        for entry in transactions
    )
    return CONFIG_TEMPLATE.format(
        **stats,
        **stats["per_block"],
        class_lines=class_lines,
        prediction_lines=prediction_lines,
        fresh_slots_per_mgas=f"{predicted['fresh_slots_per_mgas']:.2f}",
        transactions=transactions,
        target_gps=round(stats["per_block"]["gas"] / BLOCK_TIME),
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--rpc", default="https://mainnet.base.org")
    parser.add_argument("--days", type=float, default=7, help="window length ending at --end")
    parser.add_argument("--start", type=int, help="first block; overrides --days")
    parser.add_argument("--end", type=int, help="last block (default: latest)")
    parser.add_argument("--samples", type=int, default=10080, help="blocks sampled evenly across the window")
    parser.add_argument("--workers", type=int, default=4, help="concurrent batch requests")
    parser.add_argument("--cache", help="JSON-lines file of fetched blocks, reused on rerun")
    parser.add_argument("--cached-only", action="store_true", help="render all complete cached rows without RPC requests")
    parser.add_argument("--out", help="write a load-test config here")
    args = parser.parse_args()
    if args.cached_only:
        if not args.cache:
            parser.error("--cached-only requires --cache")
        with open(args.cache) as cache:
            lines = (line for line in cache if line.endswith("\n") and line.strip())
            rows = sorted((json.loads(line) for line in lines), key=lambda row: row["n"])
        validate_cache_rows(rows)
        if len(rows) < 2:
            parser.error("--cached-only requires at least two cached blocks")
        stats = report([row["n"] for row in rows], rows)
    else:
        end = args.end if args.end is not None else int(rpc_batch(args.rpc, [("eth_blockNumber", [])])[0], 16)
        start = args.start if args.start is not None else end - int(args.days * 86400 / BLOCK_TIME) + 1
        count = max(2, min(args.samples, end - start + 1))
        blocks = [start + round(i * (end - start) / (count - 1)) for i in range(count)]
        rows = sample(args.rpc, blocks, args.cache, args.workers)
        stats = report(blocks, rows)
    stats["predicted"] = predict(weights(stats["classes"]))
    print(json.dumps(stats, indent=2))
    if args.out:
        mix_targets = targets(stats, rows)
        with open(args.out, "w") as out:
            out.write(render(stats, mix_targets))
        with open(os.path.splitext(args.out)[0] + ".targets.json", "w") as out:
            out.write(json.dumps(mix_targets, indent=2) + "\n")


if __name__ == "__main__":
    main()
