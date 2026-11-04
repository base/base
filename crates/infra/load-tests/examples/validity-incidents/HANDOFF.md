# Handoff: re-running the validity-incident load tests

This is a runbook for an agent or engineer who needs to repeat the validity-transaction load
tests against Base Sepolia. The tests were first run on 2026-11-03, before mainnet Denim
activation. See `README.md` in this directory for what each scenario models.

## What you need from the user

1. **A Sepolia submission endpoint, as `SEPOLIA_RPC`.** The public Sepolia endpoint rate-limits
   far below these loads, so the original runs used a dedicated, unrate-limited endpoint
   whose URL embeds an API key. The user must supply that URL (for example from a password
   manager or a file only they can read). Export it in the shell; never commit it or write it
   into this directory, a config, a log, a PR or a report.
   ```sh
   export SEPOLIA_RPC="$(cat /path/the/user/chooses)"   # or paste it into the shell yourself
   ```
2. **A funded Sepolia private key in `~/.sepolia-funder`** (hex, mode 600). `run.sh` reads it
   from there by default. Override with `FUNDER_KEY_FILE=/other/path` or `FUNDER_KEY=0x...`.
   - It must be a plain EOA. EIP-7702 delegated accounts are limited to one in-flight
     transaction, so batched funding fails with `in-flight transaction limit reached for
     delegated accounts`. Check with `cast code <address> --rpc-url "$SEPOLIA_RPC"`: a plain
     EOA returns `0x`, a delegated one returns `0xef0100…`.
   - Balance: about 1.5 ETH covers one run of every scenario. The flood tops up senders to
     `funding_amount` and skips any already above half of it, so later runs cost less:
     - s1/s2/s5 (seed 4242, 60,000 senders × 0.000003 ETH): about 0.18 ETH;
     - s3 (seed 4343, 12,000 × 0.00002 ETH): about 0.24 ETH;
     - user traffic (seed 777001, 200 × 0.003 ETH): about 0.6 ETH per run, drained afterwards.
3. **Datadog access through the `pup` CLI** (only for `summarize.py`). Check with
   `pup auth status`.

## Build

From the repo root:

```sh
cargo build --release -p base-load-tester-bin   # produces target/release/base-load-tester
```

`run.sh` builds it if missing. Set `LOAD_TESTER_BIN` to use another binary.

## Run

```sh
cd crates/infra/load-tests/examples/validity-incidents
export SEPOLIA_RPC=...                    # supplied by the user, see above
./run.sh baseline                         # user traffic only, ~5 min; run this first
./run.sh s1-resting-flood-15s             # each flood scenario runs ~6–12 min
USER_DURATION=6m ./run.sh s3-wakeup-storm # shorten user traffic to match a shorter flood
SEPOLIA_RPC=... ./summarize.py results/<run-dir>
```

`run.sh` funds the flood senders, starts the flood, then starts ~950 tx/s of plain user
transfers once the flood is running, so you can measure what the flood does to users. It
writes `results/<scenario>-<utc>/` (gitignored), containing:

- `flood.log`, `user.log`, `flood-summary.json`, `user-summary.json`;
- `flood-start`, `user-start`, `user-end`, `end` timestamps for matching Datadog windows.

When it exits, it redacts the endpoint from those files and deletes the rendered configs.

Run scenarios one at a time: s1, s2 and s5 share seed 4242, and runs that overlap distort each
other. Leave a couple of minutes between runs so the validity pools drain (15s lifetimes clear
in seconds; s2's 60s lifetimes take about a minute).

## Read the results

- **User impact:** `user-summary.json` latency percentiles and inclusion counts. The
  baseline on 2026-11-03 was p50 396ms / p95 534ms / p99 907ms.
- **Flood:** `flood-summary.json`.
  - `errors` buckets: `timeout` or `batch send request failed` mean ingress saturation;
    `pool_full` means the validity pool cap; `already expired` means a block-number bound
    passed before admission.
  - `late_ticks` growing quickly in `flood.log` means the client host, not the network, is
    the limit. A single host tops out around 5–8k submissions/s.
- **Sequencer and mempool:** `summarize.py` prints these. The key Datadog metrics (tag
  `environment:web3-shared-prod`, `configname:sepolia-ha-sequencer-1-k8s` or
  `sepolia-reth-mempool-k8s`) are:
  - `base.reth_base_node_sequencer_block_to_block_duration` (p99 should stay near 200ms);
  - `base.reth_base_builder_resting_predicate_hold_duration` (time walking the resting set);
  - `base.reth_txpool_validity_pool_transactions` (sequencer cap is 40k);
  - `base.reth_base_builder_validity_predicate_evaluations` by `outcome`;
  - `kubernetes.cpu.usage.total` and `system.cpu.idle` for mempool saturation.

## Results from 2026-11-03 (Sepolia, v1.5.2-rc.15, Denim active)

| Run | User p50 / p95 / p99 / max | Notes |
| --- | --- | --- |
| baseline | 396ms / 534ms / 907ms / 2.5s | ~953 tx/s |
| s1 | 489ms / 3.2s / 6.3s / 14.6s | Mempool hosts saturated: ~85–90% CPU at ~2.9k submissions/s, collapsing above ~4k/s. Resting set up to 19.9k, walk max 16ms |
| s5 | not captured | Stopped early once ingress saturated; resting set 25k, walk 19ms |
| s3 | 399ms / 557ms / 813ms / 1.8s | Up to 2,291 txs in one release block, rest deferred by the 10ms predicate budget |
| s2 | 463ms / 1.0s / 1.6s / 6.0s | Sequencer validity pool pinned at its 40k cap, 127k `pool_full`; walk p99 48ms |
| s4 | not run | |

No empty blocks were observed in any run. Block-to-block p99 stayed at or below 250ms.

## Clean up

Drain the senders back to the funder when finished:

```sh
base-load-tester rescue --rpc-url "$SEPOLIA_RPC" --seed 4242 --count 60000 2>&1 | sed "s#$SEPOLIA_RPC#<redacted-rpc>#g"
base-load-tester rescue --rpc-url "$SEPOLIA_RPC" --seed 4343 --count 12000 2>&1 | sed "s#$SEPOLIA_RPC#<redacted-rpc>#g"
base-load-tester rescue --rpc-url "$SEPOLIA_RPC" --seed 777001 --count 200 2>&1 | sed "s#$SEPOLIA_RPC#<redacted-rpc>#g"
```

`rescue` reads the funder key from `FUNDER_KEY`
(`export FUNDER_KEY="$(tr -d ' \n\r' < ~/.sepolia-funder)"`) and prints the RPC URL, hence the
`sed`. Flood senders hold very little each (0.000003 ETH for seed 4242), so draining 4242 may
cost more gas than it recovers. Drain 777001 first.

## Caveats

- Sepolia is a weaker rehearsal than mainnet: 2 serving mempool nodes on 12-vCPU hosts (mainnet
  has 4 on 96-vCPU hosts with 70-core limits), no 15s lifetime cap on the mempools (mainnet
  sets `RETH_VALIDITY_MAX_EXPIRY_SECS=15`), 120M gas per block, and native-only builder mode.
  Ingress saturates long before the builder does.
- The dedicated endpoint still returned 1,439 `rate_limited` responses during s5. The source
  was not traced.
- Predicates gate on `L1Block.sequenceNumber`, which changes every block, so every parked
  transaction is re-evaluated each block and never matches. Nothing needs deploying.
