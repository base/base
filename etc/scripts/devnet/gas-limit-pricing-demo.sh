#!/usr/bin/env bash
# End-to-end demo for gas limit pricing (docs/guides/GAS_LIMIT_PRICING.md).
#
# Reproduces the problem the rule solves and shows the rule closing it, on one devnet:
#
#   1. A flood of "probe" validity transactions (10% of senders) bids a 20x priority fee,
#      declares a 500,000 gas limit, passes 64 predicates, and reverts after ~22k gas.
#      Honest validity transactions (90% of senders) bid the market tip, declare 100,000 gas,
#      pass the same predicates, and succeed (`DoubleCounter.increment2`).
#   2. Before Zenith activates (metered billing), the probes sort to the front of every
#      flashblock, consume the builder's predicate-evaluation budget
#      (`--builder.predicate-eval-hard-cutoff-ms`) and execution window, and pay the sequencer
#      `20x tip * ~22k gas` each: about what one honest transaction pays.
#   3. After Zenith activates (gas limit pricing), the same flood pays `20x tip * 500,000`
#      per probe, win or lose. The builder's validity-flow priority-fee revenue and the probe
#      cohort's fees rise by roughly `500,000 / 22,000`; the honest cohort's by `100,000 / ~45,000`.
#
# The load generator does not adapt to prices, so predicate-budget deferrals look alike in both
# phases; what the demo shows is that the behaviour that causes them stops being cheap. The
# economic response (fewer probes at high tips) is measured on mainnet, not here.
#
# Usage (from the repository root):
#   L2_BUILDER_PREDICATE_EVAL_HARD_CUTOFF_MS=2 L2_BASE_ZENITH_BLOCK=3000 just devnet up-single zenith
#   ./etc/scripts/devnet/gas-limit-pricing-demo.sh
#
# The devnet runs 200 ms blocks from Denim (block 25), so block 3000 is about 10 minutes out.
# The first phase (load-tester build, funding, 60 s of load, drain) must finish before
# activation; the script measures the block time, skips the metered phase if there is not
# enough time, and still runs the gas-limit-priced phase. The 2 ms predicate cutoff (about 40
# evaluations per block) is what lets a 200-sender flood exhaust the budget on a laptop; at the
# production default (10 ms) use `just load-test validity-stress` scale instead.
#
# Environment overrides:
#   RPC             builder RPC                     (default $L2_BUILDER_RPC_URL from common.sh)
#   PROMETHEUS_URL  devnet Prometheus               (default http://localhost:9090)
#   ZENITH_BLOCK    activation block                (default $L2_BASE_ZENITH_BLOCK or 3000)
#   OUT_DIR         where phase summaries are kept  (default: a fresh mktemp dir)
#   PHASE_SECONDS   expected wall time of one load phase incl. setup (default 180)
#   SKIP_EVEREST=1  do not run the metered phase even if there is time
#   REUSE_EVEREST=1 reuse $OUT_DIR/everest.summary.json from an earlier run instead of re-running
#                   the metered phase (for a chain that is already past activation)
#
# Requires Foundry (`cast`, `forge`), `jq`, `curl`, `python3`, `cargo`, `just`.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

RPC="${RPC:-$L2_BUILDER_RPC_URL}"
PROMETHEUS_URL="${PROMETHEUS_URL:-http://localhost:9090}"
ZENITH_BLOCK="${ZENITH_BLOCK:-${L2_BASE_ZENITH_BLOCK:-3000}}"
OUT_DIR="${OUT_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/gas-limit-pricing-demo.XXXXXX")}"
PHASE_SECONDS="${PHASE_SECONDS:-180}"
JOB="l2_builder"

for tool in cast forge jq curl python3 cargo just; do
    command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: $tool is required" >&2; exit 1; }
done
mkdir -p "$OUT_DIR"

head_block() { cast block-number --rpc-url "$RPC"; }

# Seconds per block, measured over the last `span` blocks (Denim runs 200 ms blocks, so header
# timestamps only resolve to seconds; averaging over a span gives the cadence).
block_seconds() {
    local head span=50 newer older
    head="$(head_block)"
    [ "$head" -gt "$span" ] || span="$head"
    [ "$span" -gt 0 ] || { echo 2; return; }
    newer="$(cast block "$head" --field timestamp --rpc-url "$RPC")"
    older="$(cast block "$((head - span))" --field timestamp --rpc-url "$RPC")"
    python3 -c "print(max(($newer - $older) / $span, 0.05))"
}

# Raw counter / histogram-sum totals from the builder. Counters only increase, so the
# difference between two snapshots is the activity inside a phase.
METRIC_QUERIES=(
    "cutoff_builds=sum(reth_base_builder_validity_predicate_eval_cutoff_builds_total{job=\"$JOB\"})"
    "budget_exhausted=sum(reth_base_builder_validity_predicate_evaluations_total{job=\"$JOB\",outcome=\"budget_exhausted\"})"
    "candidates_deferred=sum(reth_base_builder_validity_predicate_candidates_deferred_per_build_sum{job=\"$JOB\"})"
    "candidates_evaluated=sum(reth_base_builder_validity_predicate_candidates_evaluated_per_build_sum{job=\"$JOB\"})"
    "validity_txs_included=sum(reth_base_builder_txs_included_per_block_sum{job=\"$JOB\",flow=\"validity\"})"
    "validity_priority_fee_wei=sum(reth_base_builder_priority_fee_revenue_wei_sum{job=\"$JOB\",flow=\"validity\"})"
    "validity_gas_used=sum(reth_base_builder_tx_gas_used_per_block_sum{job=\"$JOB\",flow=\"validity\"})"
)

snapshot_metrics() {
    local out="$1" json="{}"
    for entry in "${METRIC_QUERIES[@]}"; do
        local name="${entry%%=*}" query="${entry#*=}" value
        value="$(curl -fsS --max-time 5 -G --data-urlencode "query=$query" "$PROMETHEUS_URL/api/v1/query" 2>/dev/null \
            | jq -r '.data.result[0].value[1] // "0"' 2>/dev/null || echo "unavailable")"
        json="$(jq -c --arg k "$name" --arg v "$value" '. + {($k): $v}' <<<"$json")"
    done
    printf '%s\n' "$json" >"$out"
}

run_phase() {
    local name="$1"
    echo ""
    echo "=== Phase: $name (head block $(head_block), Zenith at $ZENITH_BLOCK) ==="
    snapshot_metrics "$OUT_DIR/$name.before.json"
    LOAD_TEST_OUTPUT="$OUT_DIR/$name.summary.json" just load-test gas-limit-pricing
    snapshot_metrics "$OUT_DIR/$name.after.json"
    echo "phase $name done at head block $(head_block); summary: $OUT_DIR/$name.summary.json"
}

echo "=== Gas limit pricing demo ==="
echo "rpc=$RPC chain_id=$(cast chain-id --rpc-url "$RPC") prometheus=$PROMETHEUS_URL out=$OUT_DIR"
echo "predicate cutoff: ${L2_BUILDER_PREDICATE_EVAL_HARD_CUTOFF_MS:-10 (builder default; set L2_BUILDER_PREDICATE_EVAL_HARD_CUTOFF_MS before 'just devnet up-single zenith' to lower it)} ms"

echo ""
echo "Building the load tester (so the metered phase is not spent compiling)..."
cargo build --quiet -p base-load-tests -p base-load-tester-bin --examples

head="$(head_block)"
everest_ran=false
if [ "${REUSE_EVEREST:-0}" = "1" ] && [ -f "$OUT_DIR/everest.summary.json" ]; then
    echo "Reusing the metered phase from $OUT_DIR/everest.summary.json (REUSE_EVEREST=1)."
    everest_ran=true
elif [ "${SKIP_EVEREST:-0}" = "1" ]; then
    echo "Skipping the metered phase (SKIP_EVEREST=1)."
elif [ "$head" -ge "$ZENITH_BLOCK" ]; then
    echo "Head $head is already past Zenith ($ZENITH_BLOCK); skipping the metered phase."
    echo "For the before/after comparison start the devnet with e.g. L2_BASE_ZENITH_BLOCK=3000."
else
    # Denim switches the devnet to 200 ms blocks at block 25; measure once that cadence has settled.
    while [ "$(head_block)" -lt 80 ]; do sleep 1; done
    block_seconds="$(block_seconds)"
    remaining="$(python3 -c "print(int(($ZENITH_BLOCK - $head) * $block_seconds))")"
    echo "block time ~${block_seconds}s; ~${remaining}s until Zenith"
    if [ "$remaining" -lt "$PHASE_SECONDS" ]; then
        echo "Only ~${remaining}s until Zenith; a phase needs ~${PHASE_SECONDS}s. Skipping the metered phase."
        echo "Start the devnet with a later activation, e.g. L2_BASE_ZENITH_BLOCK=3000 at 200 ms blocks."
    else
        run_phase everest
        if [ "$(head_block)" -ge "$ZENITH_BLOCK" ]; then
            echo "WARNING: the metered phase ran past the activation block; its receipts mix both rules."
        fi
        everest_ran=true
    fi
fi

echo ""
echo "Waiting for Zenith (block $ZENITH_BLOCK) plus two blocks..."
while [ "$(head_block)" -lt "$((ZENITH_BLOCK + 2))" ]; do sleep 1; done
run_phase zenith

echo ""
OUT_DIR="$OUT_DIR" EVEREST_RAN="$everest_ran" python3 - <<'PY'
import json, os, sys

out = os.environ["OUT_DIR"]
phases = (["everest"] if os.environ["EVEREST_RAN"] == "true" else []) + ["zenith"]
PROBE, HONEST = "validity_pass_priority_lead", "validity_pass"
ETH = 10**18

def load(path):
    with open(path) as f:
        return json.load(f)

def num(x):
    try:
        return float(x)
    except (TypeError, ValueError):
        return None

def metric_delta(phase, key):
    before, after = load(f"{out}/{phase}.before.json"), load(f"{out}/{phase}.after.json")
    b, a = num(before.get(key)), num(after.get(key))
    return None if b is None or a is None else a - b

def cohorts(phase):
    summary = load(f"{out}/{phase}.summary.json")
    return {c["cohort"]: c for c in summary.get("by_cohort", [])}

failures = []
rows = {}

def dur(d):
    return d["secs"] + d["nanos"] / 1e9 if isinstance(d, dict) else float(d)

for phase in phases:
    by = cohorts(phase)
    print(f"--- {phase}: per-cohort settlement (from receipts) and inclusion latency ---")
    print(f"{'cohort':<30}{'confirmed':>10}{'reverted':>9}{'gas used':>14}{'gas limit':>14}{'tip/tx (ETH)':>15}{'fee/tx (ETH)':>15}{'p50':>8}{'p95':>8}{'max':>8}")
    for label, name in ((PROBE, "probes (20x tip, 500k limit)"), (HONEST, "honest (1x tip, 100k limit)")):
        c = by.get(label)
        if not c:
            failures.append(f"{phase}: no confirmed transactions in cohort {label}")
            continue
        n = c["confirmed"]
        tip, fee = int(c.get("total_priority_fee_wei", 0)), int(c.get("total_fee_wei", 0))
        lat = c["block_latency"]
        rows[(phase, label)] = c
        print(f"{name:<30}{n:>10}{c['reverted']:>9}{c['total_gas']:>14}{c.get('total_gas_limit', 0):>14}{tip / n / ETH:>15.9f}{fee / n / ETH:>15.9f}{dur(lat['p50']):>7.2f}s{dur(lat['p95']):>7.2f}s{dur(lat['max']):>7.2f}s")
    print(f"--- {phase}: builder (Prometheus deltas over the phase) ---")
    for key, label in (
        ("candidates_evaluated", "validity candidates evaluated"),
        ("candidates_deferred", "validity candidates deferred"),
        ("budget_exhausted", "  of which: predicate budget exhausted"),
        ("cutoff_builds", "block builds that hit the predicate cutoff"),
        ("validity_txs_included", "validity transactions included"),
        ("validity_gas_used", "validity gas billed (header accounting)"),
    ):
        d = metric_delta(phase, key)
        print(f"  {label:<48}{'unavailable' if d is None else f'{d:,.0f}'}")
    rev = metric_delta(phase, "validity_priority_fee_wei")
    print(f"  {'validity priority-fee revenue to the sequencer':<48}{'unavailable' if rev is None else f'{rev / ETH:.6f} ETH'}")
    rows[(phase, "revenue")] = rev
    print()

def tip_per_tx(c):
    return int(c.get("total_priority_fee_wei", 0)) / c["confirmed"]

def fee_per_tx(c):
    return int(c.get("total_fee_wei", 0)) / c["confirmed"]

# The problem: probes are evaluated first, so honest transactions wait.
for phase in phases:
    p, h = rows.get((phase, PROBE)), rows.get((phase, HONEST))
    if p and h:
        print(f"{phase}: inclusion latency p50 probes {dur(p['block_latency']['p50']):.2f}s vs honest {dur(h['block_latency']['p50']):.2f}s; p95 {dur(p['block_latency']['p95']):.2f}s vs {dur(h['block_latency']['p95']):.2f}s (probes are evaluated first; honest candidates wait for budget)")

# Checks on the gas-limit-priced phase.
z_probe, z_honest = rows.get(("zenith", PROBE)), rows.get(("zenith", HONEST))
if z_probe and z_honest:
    for label, c in ((PROBE, z_probe), (HONEST, z_honest)):
        if c.get("total_gas_limit") and c["total_gas"] != c["total_gas_limit"]:
            failures.append(f"zenith {label}: receipts report {c['total_gas']} gas but {c['total_gas_limit']} was declared; gas limit pricing should bill the limit")
    if z_probe["reverted"] == 0:
        failures.append("zenith probes: none reverted; the probe payload must revert")
    ratio = tip_per_tx(z_probe) / tip_per_tx(z_honest) if tip_per_tx(z_honest) else float("inf")
    print(f"zenith: a probe pays the sequencer {ratio:.1f}x the tip an honest transaction pays (expected ~ 20 * 500,000 / 100,000 = 100x); total fee incl. base fee {fee_per_tx(z_probe) / fee_per_tx(z_honest):.1f}x")
    if ratio < 50:
        failures.append(f"zenith: probe/honest tip ratio {ratio:.1f}x is below 50x")

e_probe, e_honest = rows.get(("everest", PROBE)), rows.get(("everest", HONEST))
if e_probe and z_probe:
    mult = tip_per_tx(z_probe) / tip_per_tx(e_probe) if tip_per_tx(e_probe) else float("inf")
    print(f"probe tip per transaction: metered {tip_per_tx(e_probe) / ETH:.9f} ETH -> gas limit priced {tip_per_tx(z_probe) / ETH:.9f} ETH ({mult:.1f}x; expected ~ 500,000 / gas used, about 23x)")
    if mult < 10:
        failures.append(f"probe tip rose only {mult:.1f}x; expected at least 10x")
    if e_honest:
        e_ratio = tip_per_tx(e_probe) / tip_per_tx(e_honest) if tip_per_tx(e_honest) else float("inf")
        print(f"everest: a probe paid the sequencer {e_ratio:.1f}x an honest transaction's tip for a 20x higher rank (it used ~{e_probe['total_gas'] / e_probe['confirmed'] / (e_honest['total_gas'] / e_honest['confirmed']):.2f}x the gas)")
    e_rev, z_rev = rows.get(("everest", "revenue")), rows.get(("zenith", "revenue"))
    if e_rev and z_rev:
        print(f"sequencer validity-flow priority-fee revenue: {e_rev / ETH:.6f} ETH -> {z_rev / ETH:.6f} ETH ({z_rev / e_rev:.1f}x) for the same flood")
    e_inc, z_inc = metric_delta("everest", "validity_txs_included"), metric_delta("zenith", "validity_txs_included")
    if e_inc and z_inc:
        print(f"validity transactions included in 60 s: {e_inc:,.0f} -> {z_inc:,.0f} (blocks now fill by declared gas; padding costs capacity as well as fees)")

for phase in phases:
    d = metric_delta(phase, "budget_exhausted")
    if d is not None and d == 0:
        print(f"note: {phase}: the predicate budget was never exhausted; lower L2_BUILDER_PREDICATE_EVAL_HARD_CUTOFF_MS or raise sender_count to reproduce the deferrals")

print()
if failures:
    for f in failures:
        print(f"FAIL: {f}")
    print("RESULT: FAIL")
    sys.exit(1)
print("RESULT: PASS")
PY
