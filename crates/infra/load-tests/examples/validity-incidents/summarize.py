#!/usr/bin/env python3
"""Summarize one validity-incident run: Datadog sequencer metrics + on-chain block stats.

usage: summarize.py <results-dir>   (needs SEPOLIA_RPC in env)
"""
import json, os, subprocess, sys, urllib.request, datetime as dt
from concurrent.futures import ThreadPoolExecutor

d = sys.argv[1]
rpc = os.environ["SEPOLIA_RPC"]
read = lambda n: open(os.path.join(d, n)).read().strip() if os.path.exists(os.path.join(d, n)) else None
start = read("flood-start") or read("user-start")
end = read("end") or read("user-end") or dt.datetime.utcnow().strftime("%FT%TZ")
ts = lambda s: int(dt.datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=dt.timezone.utc).timestamp())
t0, t1 = ts(start), ts(end)
SEQ = "environment:web3-shared-prod,configname:sepolia-ha-sequencer-1-k8s"
MP = "environment:web3-shared-prod,configname:sepolia-reth-mempool-k8s"


def dd(q):
    out = subprocess.run(["pup", "metrics", "query", "--query", q, "--from", str(t0), "--to", str(t1), "--output", "json"],
                         capture_output=True, text=True).stdout
    try:
        series = json.loads(out)["data"]["series"]
    except Exception:
        return {}
    res = {}
    for s in series:
        pts = [p[1] for p in s["pointlist"] if p[1] is not None]
        if pts:
            res[s.get("scope", "")] = (sum(pts) / len(pts), max(pts))
    return res


Q = {
    "block_to_block p99 (s)": f"max:base.reth_base_node_sequencer_block_to_block_duration.quantile{{{SEQ},quantile:0.99}}",
    "block_to_block max (s)": f"max:base.reth_base_node_sequencer_block_to_block_duration.quantile{{{SEQ},quantile:1.0}}",
    "seal_pipeline p99 (s)": f"max:base.reth_base_node_sequencer_seal_pipeline_duration.quantile{{{SEQ},quantile:0.99}}",
    "ticker_drift p99 (s)": f"max:base.reth_base_node_sequencer_ticker_drift_seconds.quantile{{{SEQ},quantile:0.99}}",
    "payload resolve p99 (s)": f"max:base.reth_payloads_resolve_duration_seconds.quantile{{{SEQ},quantile:0.99}}",
    "resting carried p99": f"max:base.reth_base_builder_resting_predicate_carried.quantile{{{SEQ},quantile:0.99}}",
    "resting carried max": f"max:base.reth_base_builder_resting_predicate_carried.quantile{{{SEQ},quantile:1.0}}",
    "resting hold p99 (s)": f"max:base.reth_base_builder_resting_predicate_hold_duration.quantile{{{SEQ},quantile:0.99}}",
    "resting hold max (s)": f"max:base.reth_base_builder_resting_predicate_hold_duration.quantile{{{SEQ},quantile:1.0}}",
    "resting begin_block p99 (s)": f"max:base.reth_base_builder_resting_predicate_begin_block_duration.quantile{{{SEQ},quantile:0.99}}",
    "resting sweep max (s)": f"max:base.reth_base_builder_resting_predicate_sweep_duration.quantile{{{SEQ},quantile:1.0}}",
    "sweep wait timeouts": f"sum:base.reth_base_builder_resting_predicate_sweep_wait_timeout.count{{{SEQ}}}.as_count()",
    "predicate eval/block p99 (s)": f"max:base.reth_base_builder_validity_predicate_eval_duration_per_block.quantile{{{SEQ},quantile:0.99}}",
    "candidates evaluated/build p99": f"max:base.reth_base_builder_validity_predicate_candidates_evaluated_per_build.quantile{{{SEQ},quantile:0.99}}",
    "eval coverage/build p50": f"max:base.reth_base_builder_validity_predicate_evaluation_coverage_per_build.quantile{{{SEQ},quantile:0.5}}",
    "seq validity pool txs": f"max:base.reth_txpool_validity_pool_transactions{{{SEQ}}}",
    "mempool validity pool txs (per node)": f"max:base.reth_txpool_validity_pool_transactions{{{MP}}}",
    "seq validity admitted/s by outcome": f"sum:base.reth_txpool_validity_admitted.count{{{SEQ}}} by {{outcome}}.as_rate()",
    "mempool validity admitted/s by outcome": f"sum:base.reth_txpool_validity_admitted.count{{{MP}}} by {{outcome}}.as_rate()",
    "seq validity evicted/s by reason": f"sum:base.reth_txpool_validity_evicted.count{{{SEQ}}} by {{reason}}.as_rate()",
    "txs included/block p50 by flow": f"max:base.reth_base_builder_txs_included_per_block.quantile{{{SEQ},quantile:0.5}} by {{flow}}",
    "seq cpu (cores)": f"sum:kubernetes.cpu.usage.total{{{SEQ}}}",
}
with ThreadPoolExecutor(8) as ex:
    results = dict(zip(Q, ex.map(dd, Q.values())))

print(f"# {os.path.basename(d.rstrip('/'))}  window {start} .. {end} ({t1 - t0}s)\n")
print("## Datadog (sequencer unless noted): mean over window / max\n")
for k, r in results.items():
    if not r:
        print(f"- {k}: no data")
        continue
    for scope, (mean, mx) in r.items():
        tag = ",".join(p for p in scope.split(",") if p.split(":")[0] in ("outcome", "reason", "flow", "pod_name"))
        if "cpu" in k:
            mean, mx = mean / 1e9, mx / 1e9
        print(f"- {k}{' [' + tag + ']' if tag else ''}: {mean:.4g} / {mx:.4g}")


def call(batch):
    body = json.dumps(batch).encode()
    req = urllib.request.Request(rpc, body, {"content-type": "application/json", "user-agent": "curl/8"})
    return json.load(urllib.request.urlopen(req, timeout=60))


def block(tag):
    return call([{"jsonrpc": "2.0", "id": 1, "method": "eth_getBlockByNumber", "params": [tag, False]}])[0]["result"]


# Binary search block numbers for the window by timestamp.
def find(t):
    lo, hi = 1, int(block("latest")["number"], 16)
    while lo < hi:
        mid = (lo + hi) // 2
        if int(block(hex(mid))["timestamp"], 16) < t:
            lo = mid + 1
        else:
            hi = mid
    return lo


b0, b1 = find(t0), find(t1)
nums = list(range(b0, b1))
batches = [nums[i:i + 100] for i in range(0, len(nums), 100)]
blocks = []
with ThreadPoolExecutor(8) as ex:
    for res in ex.map(lambda b: call([{"jsonrpc": "2.0", "id": n, "method": "eth_getBlockByNumber", "params": [hex(n), False]} for n in b]), batches):
        blocks += [r["result"] for r in res if r.get("result")]
blocks.sort(key=lambda b: int(b["number"], 16))
print(f"\n## Chain: blocks {b0}..{b1} ({len(blocks)} blocks, expected {(t1 - t0) * 5})\n")
print("minute | blocks | gas/block avg (M) | gas/block max (M) | txs/block avg | blocks <=1 tx | base fee (gwei) end")
buckets = {}
for b in blocks:
    buckets.setdefault((int(b["timestamp"], 16) - t0) // 60, []).append(b)
for m, bs in sorted(buckets.items()):
    g = [int(b["gasUsed"], 16) / 1e6 for b in bs]
    n = [len(b["transactions"]) for b in bs]
    print(f"{m:>6} | {len(bs):>6} | {sum(g) / len(g):>17.1f} | {max(g):>17.1f} | {sum(n) / len(n):>13.1f} | {sum(1 for x in n if x <= 1):>13} | {int(bs[-1]['baseFeePerGas'], 16) / 1e9:.4f}")

for name in ("user-summary.json", "flood-summary.json"):
    p = os.path.join(d, name)
    if os.path.exists(p):
        print(f"\n## {name}\n")
        print(open(p).read()[:4000])
