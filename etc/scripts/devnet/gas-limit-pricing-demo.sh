#!/usr/bin/env bash
# End-to-end demo for gas limit pricing (docs/guides/GAS_LIMIT_PRICING.md).
#
# Reproduces the problem the rule solves and shows the rule closing it, on one devnet:
#
#   1. A flood of "probe" validity transactions (30% of senders) bids a 20x priority fee,
#      declares a 2,000,000 gas limit, passes 64 storage predicates, and reverts after ~22k gas.
#      Honest validity transactions (70% of senders) bid the market tip, declare 100,000 gas,
#      pass the same predicates, and succeed (`DoubleCounter.increment2`).
#   2. Before Zenith activates (metered billing), the probes sort to the front of every
#      flashblock, consume the builder's predicate-evaluation budget
#      (`--builder.predicate-eval-hard-cutoff-ms`) and execution window, and pay the sequencer
#      `20x tip * ~22k gas` each: about what one honest transaction pays.
#   3. After Zenith activates (gas limit pricing), the same flood pays `20x tip * 2,000,000`
#      per probe, win or lose. The builder's validity-flow priority-fee revenue and the probe
#      cohort's fees rise by roughly `2,000,000 / 22,000`; the honest cohort's by `100,000 / ~45,000`.
#
# The load generator does not adapt to prices, so predicate-budget deferrals look alike in both
# phases; what the demo shows is that the behaviour that causes them stops being cheap. The
# economic response (fewer probes at high tips) is measured on mainnet, not here.
#
# Usage (from the repository root):
#   L2_BUILDER_PREDICATE_EVAL_HARD_CUTOFF_MS=2 L2_BASE_ZENITH_BLOCK=400 just devnet up-single zenith
#   ./etc/scripts/devnet/gas-limit-pricing-demo.sh
#
# The first phase must finish before activation, so schedule Zenith at least ~400 blocks
# (~800 s at 2 s/block) out; the script skips the metered phase if there is not enough time
# and still runs the gas-limit-priced phase. Lowering the predicate cutoff lets a 400-sender
# flood exhaust the budget on a laptop; at the production default (10 ms) use
# `just load-test validity-stress` scale instead.
#
# Environment overrides:
#   RPC             builder RPC                     (default $L2_BUILDER_RPC_URL from common.sh)
#   PROMETHEUS_URL  devnet Prometheus               (default http://localhost:9090)
#   ZENITH_BLOCK    activation block                (default $L2_BASE_ZENITH_BLOCK or 100)
#   OUT_DIR         where phase summaries are kept  (default: a fresh mktemp dir)
#   PHASE_SECONDS   expected wall time of one load phase incl. setup (default 150)
#   SKIP_EVEREST=1  do not run the metered phase even if there is time
#
# Requires Foundry (`cast`, `forge`), `jq`, `curl`, `python3`, `cargo`, `just`.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

RPC="${RPC:-$L2_BUILDER_RPC_URL}"
PROMETHEUS_URL="${PROMETHEUS_URL:-http://localhost:9090}"
ZENITH_BLOCK="${ZENITH_BLOCK:-${L2_BASE_ZENITH_BLOCK:-100}}"
OUT_DIR="${OUT_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/gas-limit-pricing-demo.XXXXXX")}"
PHASE_SECONDS="${PHASE_SECONDS:-150}"
BLOCK_SECONDS=2
JOB="l2_builder"

for tool in cast forge jq curl python3 cargo just; do
    command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: $tool is required" >&2; exit 1; }
done
mkdir -p "$OUT_DIR"

head_block() { cast block-number --rpc-url "$RPC"; }

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
if [ "${SKIP_EVEREST:-0}" = "1" ]; then
    echo "Skipping the metered phase (SKIP_EVEREST=1)."
elif [ "$head" -ge "$ZENITH_BLOCK" ]; then
    echo "Head $head is already past Zenith ($ZENITH_BLOCK); skipping the metered phase."
    echo "For the before/after comparison start the devnet with e.g. L2_BASE_ZENITH_BLOCK=400."
else
    remaining=$(( (ZENITH_BLOCK - head) * BLOCK_SECONDS ))
    if [ "$remaining" -lt "$PHASE_SECONDS" ]; then
        echo "Only ~${remaining}s until Zenith; a phase needs ~${PHASE_SECONDS}s. Skipping the metered phase."
    else
        run_phase everest
        everest_ran=true
    fi
fi

echo ""
echo "Waiting for Zenith (block $ZENITH_BLOCK) plus two blocks..."
while [ "$(head_block)" -lt "$((ZENITH_BLOCK + 2))" ]; do sleep 2; done
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
for phase in phases:
    by = cohorts(phase)
    print(f"--- {phase}: per-cohort settlement (from receipts) ---")
    print(f"{'cohort':<30}{'confirmed':>10}{'reverted':>10}{'gas used':>16}{'gas limit':>16}{'fee (ETH)':>14}{'fee/tx (ETH)':>16}")
    for label, name in ((PROBE, "probes (20x tip, 2M limit)"), (HONEST, "honest (1x tip, 100k limit)")):
        c = by.get(label)
        if not c:
            failures.append(f"{phase}: no confirmed transactions in cohort {label}")
            continue
        fee = int(c.get("total_fee_wei", 0))
        per_tx = fee / c["confirmed"] if c["confirmed"] else 0
        rows[(phase, label)] = c
        print(f"{name:<30}{c['confirmed']:>10}{c['reverted']:>10}{c['total_gas']:>16}{c.get('total_gas_limit', 0):>16}{fee / ETH:>14.6f}{per_tx / ETH:>16.9f}")
    print(f"--- {phase}: builder (Prometheus deltas over the phase) ---")
    for key, label in (
        ("candidates_evaluated", "validity candidates evaluated"),
        ("candidates_deferred", "validity candidates deferred"),
        ("budget_exhausted", "  of which: predicate budget exhausted"),
        ("cutoff_builds", "flashblock builds that hit the predicate cutoff"),
        ("validity_txs_included", "validity transactions included"),
        ("validity_gas_used", "validity gas billed (header accounting)"),
    ):
        d = metric_delta(phase, key)
        print(f"  {label:<48}{'unavailable' if d is None else f'{d:,.0f}'}")
    rev = metric_delta(phase, "validity_priority_fee_wei")
    print(f"  {'validity priority-fee revenue to the sequencer':<48}{'unavailable' if rev is None else f'{rev / ETH:.6f} ETH'}")
    rows[(phase, "revenue")] = rev
    print()

# Checks.
z_probe, z_honest = rows.get(("zenith", PROBE)), rows.get(("zenith", HONEST))
if z_probe and z_honest:
    for label, c in ((PROBE, z_probe), (HONEST, z_honest)):
        if c.get("total_gas_limit") and c["total_gas"] != c["total_gas_limit"]:
            failures.append(f"zenith {label}: receipts report {c['total_gas']} gas but {c['total_gas_limit']} was declared; gas limit pricing should bill the limit")
    if z_probe["reverted"] == 0:
        failures.append("zenith probes: none reverted; the probe payload must revert")
    probe_per_tx = int(z_probe["total_fee_wei"]) / z_probe["confirmed"]
    honest_per_tx = int(z_honest["total_fee_wei"]) / z_honest["confirmed"]
    ratio = probe_per_tx / honest_per_tx if honest_per_tx else float("inf")
    print(f"zenith: a probe pays {ratio:.1f}x what an honest transaction pays (expected ~ 20 * 2,000,000 / 100,000 = 400x)")
    if ratio < 100:
        failures.append(f"zenith: probe/honest fee ratio {ratio:.1f}x is below 100x")

e_probe = rows.get(("everest", PROBE))
if e_probe and z_probe:
    e_per_tx = int(e_probe["total_fee_wei"]) / e_probe["confirmed"]
    z_per_tx = int(z_probe["total_fee_wei"]) / z_probe["confirmed"]
    mult = z_per_tx / e_per_tx if e_per_tx else float("inf")
    print(f"probe cost per transaction: metered {e_per_tx / ETH:.9f} ETH -> gas limit priced {z_per_tx / ETH:.9f} ETH ({mult:.1f}x; expected ~ 2,000,000 / gas used)")
    if mult < 20:
        failures.append(f"probe cost rose only {mult:.1f}x; expected at least 20x")
    e_h = rows.get(("everest", HONEST))
    if e_h:
        e_ratio = (e_per_tx) / (int(e_h["total_fee_wei"]) / e_h["confirmed"])
        print(f"everest: a probe paid {e_ratio:.1f}x what an honest transaction paid, for a 20x higher rank")
    e_rev, z_rev = rows.get(("everest", "revenue")), rows.get(("zenith", "revenue"))
    if e_rev and z_rev:
        print(f"sequencer validity-flow priority-fee revenue: {e_rev / ETH:.6f} ETH -> {z_rev / ETH:.6f} ETH ({z_rev / e_rev:.1f}x) for the same flood")

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
