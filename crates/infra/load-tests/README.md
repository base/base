# Load Tests

Load testing and benchmarking framework for Base infrastructure.

## Crate

| Crate | Description |
|-------|-------------|
| `base-load-tests` | Core library with workload generation, transaction submission, and metrics collection |
| `base-load-tester-bin` | Binary crate for running load tests and rescue/drain commands |

## Goals

- Provide standardized transaction submission for network load testing
- Centralize workload generation, network orchestration, and metrics collection
- Enable reproducible test scenarios with deterministic configurations

## Quick Start

```bash
# Run load test against local devnet (uses Anvil Account #1)
just load-test run

# Deploy the devnet WETH/USDC harness and run real-token swaps
just load-test real-token

# Deploy DoubleCounter and run the 64-predicate validity stress profile
just load-test validity-stress

# Run real-token swaps against a network with predeployed contracts
FUNDER_KEY=0x... just load-test real-token sepolia

# Swap real-token balances back to WETH, unwrap, and drain ETH
FUNDER_KEY=0x... just load-test real-token-recover sepolia

# Run load test against sepolia (requires funded key)
FUNDER_KEY=0x... just load-test run sepolia
```

Or run directly with cargo:

```bash
# Build the crates
cargo build -p base-load-tests -p base-load-tester-bin

# Run tests
cargo test -p base-load-tests

# Run the load test binary with a config file
cargo run -p base-load-tester-bin --bin base-load-tester -- path/to/config.yaml
```

## Configuration

All configuration is done via YAML files. The runner uses a single adaptive open-loop submission
mode (confirmation-backlog-aware pacing); there is no separate closed-loop mode.
See `src/config/test_config.rs` for comprehensive field documentation, or
`examples/devnet.yaml` for a working example.
Example minimal config:

```yaml
transaction_submission_rpcs:
  - "http://localhost:8545"
# Add more URLs to shard submit batches across multiple HTTP endpoints.
batch_size: 100
query_rpc: "http://localhost:8545"
# Optional: clear pending transactions from these admin RPC nodes for all sender addresses.
txpool_nodes: []
sender_count: 10
target_gps: 2100000
# Align canonical block polling and convert target_gps into a per-block gas floor.
block_time: "2s"
duration: "30s"
```

Ordinary invocations calibrate and run in one go. Benchmark harnesses that need an explicit
ready/start handshake before measured submission can pass `--separate-setup <control-dir>` and
`--block-gas-limit <gas>` on the command line; these orchestration controls are intentionally not
part of the portable YAML configuration.

`in_flight_per_sender` bounds unconfirmed transactions per sender and defaults to `16`, matching
Reth's default per-account transaction-pool slots. It remains configurable for nodes with a
different pool policy. The aggregate cap defaults to
`in_flight_per_sender * sender_count`. Set `max_total_in_flight` to cap the aggregate independently
of sender count, e.g. to protect a shared target node's mempool regardless of how many senders are
configured.

`in_flight_per_sender` and `max_total_in_flight` bound unconfirmed *transactions*, not outbound
*requests*. If the submission RPC is rate-limiting you (e.g. `over rate limit` failures) rather than
its mempool overflowing, set `max_concurrent_submit_requests` instead: it caps how many
`eth_sendRawTransaction` batch requests may be outstanding to the submission RPC(s) at once, without
shrinking the in-flight inventory target. When set, it also expands the signer and sender worker
pools so the configured request concurrency can be reached. The shared semaphore remains the
authoritative outbound request limit. `batch_size` controls the maximum transactions per JSON-RPC
batch request and defaults to 100.

During measurement, the runner refills immediately after an inclusion source releases transaction
inventory. When `flashblocks_ws` is configured, builder broadcasts provide the earliest signal;
phase-locked canonical polling remains active as an automatic fallback and the authoritative source
for final metrics. Both sources feed the same idempotent depth controller, so canonical observation
does not double-release transactions already seen in a flashblock.

The controller calibrates expected execution gas before measurement, targets
`target_gps * block_time` of that estimated gas outstanding, and permits up to twice that depth
while confirmed gas is behind the run-average target. Transaction gas limits remain unchanged for
execution safety, but do not reduce TPS when they conservatively exceed observed gas usage. Refills
measure depth from transactions accepted by a submission RPC; local submission backlog still counts
toward sender and aggregate transaction capacity but is not treated as node mempool inventory.
Refills are capped by the cumulative measured submission budget (`target_gps * elapsed`), so faster
flashblock inclusion cannot drive offered load above the configured rate. When `target_gps` is
omitted, the floor is one full block and the ceiling is two full blocks. Capacity and submission
bottlenecks are reported without failing the run. Omit `flashblocks_ws` to run with canonical
polling only; removing the flashblock watcher does not change the controller or submission pipeline.
The final pacing summary reports canonical, flashblock, and safety refill-cycle counts so source
fallback is visible.

### Logging

The CLI defaults to INFO logs for the load-test crates and WARN logs for dependencies. In an
interactive terminal, a compact live footer stays below the logs through setup, submission, and
confirmation draining. Redirected and non-interactive runs emit the same five-second structured
progress events without terminal control sequences.

Use `RUST_LOG=base_load_tests=debug` for pacing diagnostics. Normal per-transaction and per-account
events are available only at trace level. Avoid trace logging for sustained load runs because its
volume scales with transaction count.

RPC and WebSocket credentials, full endpoint URLs, deterministic account seeds, and randomized
recipient recovery values are excluded from structured logs. Fresh-recipient recovery instructions
are still printed explicitly at startup, so protect captured stdout/stderr as recovery material.

`transaction_submission_rpcs` accepts either a single URL string or a list; submit batches are
distributed across the configured HTTP endpoints.
`txpool_nodes` is optional and defaults to an empty list; when present, the load tester calls
`admin_dropSenderTransactions` for every sender address on every configured node before funding.
Canonical transaction landing is detected by phase-locking `query_rpc` polling to `block_time`, then
probing briefly until the next `eth_getBlockByNumber` response becomes available. Submitted hashes
are matched against each block's transaction list. Gas usage and revert status are backfilled
in a single `eth_getBlockReceipts` batch pass at the very end of the run, scoped only to the blocks
that contained our transactions, so `query_rpc` must support `eth_getBlockReceipts`. Receipt-fetch
delay is measured for logging but is no longer included in the JSON output.

### Available Configs

| Config | Target | Notes |
|--------|--------|-------|
| `devnet.yaml` | Local devnet | Uses Anvil Account #1 |
| `validity-devnet.yaml` | Local devnet | Validity (conditional) workload; routes half the senders through `base_sendRawTransactionValidity`. Run with `FUNDER_KEY=... just load-test run validity-devnet`. Requires the node validity flags for end-to-end enforcement |
| `real-token-devnet.yaml.template` | Local devnet | Rendered by `just load-test real-token` after deploying the devnet WETH/USDC harness |
| `validity-stress.yaml.template` | Local devnet | Rendered by `just load-test validity-stress` with a freshly deployed `DoubleCounter` |
| `sepolia.yaml` | Base Sepolia | Requires `FUNDER_KEY` |
| `real-token-sepolia.yaml` | Base Sepolia | Uses predeployed WETH/USDC and the Uniswap V3 swap router; run with `just load-test real-token sepolia`; recover with `just load-test real-token-recover sepolia` |
| `real-token-mainnet-snapshot.yaml` | Local/shadow Base mainnet snapshot | Wraps funded ETH into WETH, acquires USDC, then runs random-direction Uniswap V3 and Aerodrome CL swaps; run with `just load-test real-token mainnet-snapshot` |
| `zeronet.yaml` | Base Zeronet | Requires `FUNDER_KEY` |

### Contract Addresses

Contract addresses for swap testing and related tokens.

#### Base Sepolia (Chain ID: 84532)

| Contract | Address |
|----------|---------|
| Uniswap V3 Router | `0x94cC0AaC535CCDB3C01d6787D6413C739ae12bc4` |
| Load Test Token A (LTTA) | `0x15948C3043A980A8d980d4D615A5E4c9514B0D64` |
| Load Test Token B (LTTB) | `0x4dc9ccF2C5A346c4032B648006B4774Ad2a021c4` |

#### Base Zeronet (Chain ID: 763360)

| Contract | Address |
|----------|---------|
| Uniswap V3 Router | `0x94cC0AaC535CCDB3C01d6787D6413C739ae12bc4` |
| Load Test Token A (LTTA) | `0x27589a9836dd2150036829120f092ad38a0b3740` |
| Load Test Token B (LTTB) | `0xc411b5f78fadab5880a287f21bb7997a192975f3` |

These tokens are deployed via `DeployTestTokenPair.s.sol` and use `FreeTransferERC20` which allows permissionless minting for load testing.

#### Base Mainnet Snapshot (Chain ID: 8453)

The `real-token-mainnet-snapshot.yaml` example is for local or shadow-builder environments restored from a Base mainnet snapshot. Do not point it at public Base mainnet RPCs with a real key.

The Sepolia real-token example is Uniswap-only. Aerodrome Slipstream's Sepolia router from `examples/sepolia.yaml` is deployed at `0xD75e6a0C801F24ebb3125E360a5A064f6b9FEFaC`, but its factory does not have a WETH/USDC pool, so adding an Aerodrome WETH/USDC leg will revert until that pool is deployed.

| Contract | Address |
|----------|---------|
| WETH | `0x4200000000000000000000000000000000000006` |
| USDC | `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913` |
| Uniswap V3 `SwapRouter02` | `0x2626664c2603336E57B271c5C0b26F421741e481` |
| Aerodrome CL Router | `0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5` |

### Environment Variables

- `FUNDER_KEY` - Private key (0x-prefixed hex) of a funded account to distribute test funds from

### Transaction Types

The config supports weighted transaction mixes:

```yaml
transactions:
  - weight: 70
    type: transfer
  - weight: 20
    type: calldata
    max_size: 256
    repeat_count: 1  # Optional: repeat for compressible data
  - weight: 10
    type: precompile
    target: sha256
```

#### Precompile Testing

All EVM precompiles are supported for load testing:

**Cryptographic**: `ecrecover`, `sha256`, `ripemd160`, `blake2f`
**Elliptic Curve**: `bn254_add`, `bn254_mul`, `bn254_pairing`
**Other**: `identity`, `modexp`, `kzg_point_evaluation`

```yaml
# Simple precompile call
- type: precompile
  target: sha256

# Blake2f with custom rounds
- type: precompile
  target: blake2f
  rounds: 50000

# Multiple calls per transaction (requires looper_contract)
- type: precompile
  target: ecrecover
  iterations: 50

# When using iterations > 1, specify looper contract address:
looper_contract: "0x..."  # Deployed PrecompileLooper contract
```

The `PrecompileLooper` contract enables batch testing by calling a precompile multiple times in a single transaction, useful for scenarios like multi-signature verification or repeated hash operations.

#### B-20 Token Testing

B-20 precompile tokens can be load-tested to benchmark the precompile's `transfer` performance.
Each sender creates and owns its own B-20 token: during setup every sender sends one `createB20`
factory tx (in parallel) whose privileged init calls grant the sender `BURN_ROLE` and mint its
supply, during the load phase each sender transfers its own token, and during teardown each sender
burns its remaining balance. A fresh per-run salt keeps each run's token addresses distinct.

Requires Beryl activation (B-20 factory and token features must be active on the target chain).

```yaml
# Each sender creates and transfers its own B-20 token per run
transactions:
  - weight: 100
    type: b20
```

#### Swap Testing

Swap payloads randomly choose direction for each generated transaction, alternating between `token_in → token_out` and `token_out → token_in`.

`real_token_setup` runs a pre-test phase before the measured loop: it wraps sender ETH into WETH, acquires the paired token through the configured acquisition route if the sender's balance is below `amount_per_sender`, and approves all measured routers for both tokens. When present and enabled, it replaces fixture-token minting (`swap_token_amount`).

```yaml
real_token_setup:
  enabled: true
  allow_chain_id_8453: true
  weth: "0x4200000000000000000000000000000000000006"
  weth_amount_per_sender: "50000000000000000"
  pair_token:
    token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    amount_per_sender: "10000000"
    acquisition:
      type: uniswap_v3_exact_input
      router: "0x2626664c2603336E57B271c5C0b26F421741e481"
      fee: 500
      amount_in: "10000000000000000"
      min_amount_out: "0"
```

`reverse_min_amount` and `reverse_max_amount` on `uniswap_v3` and `aerodrome_cl` set the amount range for `token_out → token_in` swaps. Use these when the two tokens have different decimal scales; when omitted, the reverse range matches the forward range.

#### Running multiple load tests

- Tune `target_gps`, `block_time`, and sender count appropriately. Omit `target_gps` to keep one
  to two block-gas-limits of inventory.

#### Account Create

By default, transfer recipients are picked from the bounded sender pool, so long runs keep targeting the same `sender_count` addresses. Set `fresh_recipient_ratio` to a value from `0.0` to `1.0` to derive that fraction of recipient signing keys from the same derivation kind as the sender pool (mnemonic when set, otherwise seed-based). This drives account-trie fan-out for workloads like the account-create performance baseline.

```yaml
fresh_recipient_ratio: 1.0
transactions:
  - weight: 100
    type: transfer
```

Recipient keys are always positioned with a runtime-random seed/offset (never the configured `seed`), so repeated runs never regenerate the same "fresh" addresses. The runner logs `randomized_recipient_seed`/`randomized_recipient_offset` at startup and writes `fresh_recipient_count` to the final summary. Recover recipients from that logged value with `AccountPool::from_mnemonic(mnemonic, fresh_recipient_count, randomized_recipient_offset)` or `AccountPool::with_offset(randomized_recipient_seed, fresh_recipient_count, recipient_offset)`.

#### Validity (Conditional) Transactions

`just load-test validity-stress [--continuous ...]` installs and builds the Foundry fixtures,
deploys `DoubleCounter` to the already-running devnet on port 7545, renders a temporary config, and
removes that config on exit. It does not restart the devnet. The profile attaches the maximum 64
storage predicates: 63 always-true reads of distinct slots 1–63 followed by
`(slot 0 & 1) == sender_parity`, where `sender_parity` is the low bit of each sender address. This
keeps approximately half the sender streams matching and half parked at either slot value. All 800
senders target the same contract and attach predicates. Ten percent use twice the baseline priority
tip while the bulk uses half the baseline tip. The 600M gas/s target fills the controller's two-block
mempool ceiling with roughly 4,500 validity transactions. An independent devnet account mutates slot
0 every 30 seconds while measured transactions increment slot 1, so the two sender halves swap
between matching and parked. That periodically wakes and rescans the parked set without measured
transactions self-invalidating the parity gate. Override the cadence with
`VALIDITY_STRESS_MUTATOR_INTERVAL_SECONDS`. This creates a shared-slot adversary where transactions
repeatedly park, wake, invalidate, and rescan while each evaluation performs the maximum number of
distinct storage reads.

With the workload running, verify sustained pressure from the repository root:

```bash
etc/scripts/devnet/validity-stress-gate.sh --wait
```

The gate requires cutoff pressure, inclusions, storage reads, parking wakeups, and rescans to hold
continuously for 60 seconds.

The stress profile submits directly to the builder RPC on port 7545 so ingress forwarding cannot
become the bottleneck or leave an asynchronous forwarding backlog between runs. To exercise the
end-to-end forwarding path instead, override `transaction_submission_rpcs` in a rendered copy to
port 8545. Both nodes still require the experimental validity flags described below.

A configurable fraction of *senders* can route their entire traffic through the
`base_sendRawTransactionValidity` endpoint, attaching validity predicates to
every transaction they submit. All four server predicate types are supported:
the state-based `balance` and `storage` conditions, and the build-position
`block_number` and `flashblock_index` conditions (compared against the block and
flashblock currently being built). This exercises the sequencer and builder
under congestion when validity predicates are in play. Set `validity.ratio` to
`0.0` (the default) to disable the workload entirely, in which case behavior is
identical to a plain run.

Routing is deterministic and *per sender* (a hash of `seed + sender`), not per
transaction, so a given sender's entire nonce stream stays on one submission
origin. This keeps nonces contiguous and single-origin, avoiding transient
nonce gaps that a split origin could cause under congestion. Because senders are
exercised roughly uniformly, the fraction of senders on the validity path
approximates the fraction of transactions.

```yaml
validity:
  ratio: 0.25                 # fraction of senders routed to the validity endpoint
  priority_lead_ratio: 0.10   # fraction of validity senders priced ahead of plain traffic
  priority_lead_multiplier: 2 # multiply the priority-lead cohort's tip
  priority_fee_divisor: 2     # lower the remaining validity senders' tips
  predicates:
    - type: balance
      address: sender          # sender | recipient | 0x-literal
      op: ">="
      value: "0"
    - type: storage
      address: "0x1234567890123456789012345678901234567890"
      slot:
        kind: fixed
        value: "0x1"
      mask: "0xff"             # optional; defaults to all ones server-side
      op: "="
      value: sender_parity      # or a fixed hex/decimal value
    # balanceOf(sender) against a seeded token's mapping slot:
    - type: storage
      address: "0xTOKEN000000000000000000000000000000000000"
      slot:
        kind: mapping
        mapping_slot: "0x0"
        key: sender
      op: ">="
      value: "0x0"
    # build-position predicates read the block/flashblock being built:
    - type: block_number
      op: ">="
      value: "0x0"                # absolute block number
    # ...or a runtime-resolved offset (current_block + offset at prepare time):
    - type: block_number
      op: ">="
      offset: "10"
    - type: flashblock_index
      op: ">="
      value: "1"
```

Predicate addresses resolve per transaction: `sender` → the tx `from`,
`recipient` → the tx `to` (falling back to `from` for contract creation), or a
fixed `0x` address. Storage slots are either a `fixed` slot or a `mapping`
slot, which computes the Solidity mapping slot `keccak256(key ++ mapping_slot)`
so `balanceOf(key)` slots are expressible. The `flashblock_index` predicate
carries an `op` and `value`, and `block_number` carries an `op` plus exactly one
of `value` (a fixed absolute block number) or `offset` (resolved to
`current_block + offset` at prepare time); both read the build position rather
than any address or slot. Storage predicate values may also be `sender_parity`,
which resolves to the low bit of each transaction sender's address. Fixed values,
slots, masks, and offsets accept hex (`0x...`) or decimal strings. At most 64
predicates may be attached per transaction.

The final summary's `by_cohort` breakdown reports confirmed transactions split
across the `plain` and `validity_pass` cohorts, so plain traffic can be compared
against validity traffic when the workload is enabled.

##### Delayed (fixed future) validity spike

To make the whole validity cohort become valid at the same future block —
parking it in the pool until then and releasing it as one predictable spike —
attach a lower-bound `block_number` predicate targeting a future block.

**Recommended: self-configuring `offset` form.** Give the `block_number`
predicate an `offset` instead of an absolute `value`. The runner resolves it to
`current_block + offset` once per prepare round, reading the chain height as
each round of transactions is prepared, so it automatically accounts for the
variable number of funding/setup blocks that run before measured submission
begins:

```yaml
validity:
  ratio: 1.0
  predicates:
    - type: block_number
      op: ">="
      offset: "10"     # resolves to current_block + 10 at prepare time
```

**Manual alternative: absolute `value` form.** You may instead hand-pick an
absolute future block number:

```yaml
validity:
  ratio: 1.0
  predicates:
    - type: block_number
      op: ">="
      value: "12345"   # a block that is still in the future when submission starts
```

Either way, every validity transaction carries a lower-bound `block_number`
predicate, so the builder skips them until the target block and then includes the
accumulated backlog together. With the absolute form, choose the target relative
to the block height **at which measured submission begins**, not run-invocation
time: account funding and token setup run first and advance the chain by a
variable number of blocks, so a target that is too low will already be satisfied
by the time submission starts (no spike). Pick a value comfortably beyond the
expected setup duration. The `offset` form avoids this guesswork. Either way,
confirm the spike landed via the `by_cohort` / `fullest_block` breakdown in the
summary. Exactly one of `value` or `offset` may be set on a `block_number`
predicate; setting both or neither is a configuration error.

**Required flags for end-to-end evaluation.** For predicates to actually be
evaluated (not merely transported), the target environment must be configured so
that:

1. The ingress/sequencer node is started with
   `--enable-experimental-validity-transactions`. This flag hard-requires
   transaction forwarding, so it must be accompanied by `--enable-tx-forwarding`
   and at least one `--builder-rpc-urls=<url>`; the node refuses to start
   otherwise. Only with this flag set is the `base_sendRawTransactionValidity`
   endpoint registered.
2. The builder is started with
   `--builder.enable-experimental-validity-transactions`. That flag both
   registers `base_sendRawTransactionValidity` on the builder and accepts
   forwarded validity metadata. If it is not set, forwarded transactions that
   carry predicates are **rejected** ("transaction extensions are disabled"), so
   a misconfiguration fails loudly rather than silently dropping predicates.
3. The builder runs the flashblocks build path (the only builder path wired in
   the shipped binaries), which is where predicates are evaluated against state.

If `validity.ratio > 0` but the ingress endpoint does not serve
`base_sendRawTransactionValidity`, the run fails loudly at startup rather than
silently degrading to plain submission.

**Interpreting the results.** There is no validity-specific builder rejection
metric, so a transaction whose predicate is false is skipped by the builder and
simply never confirms (it is not distinguishable from an ordinary drop by a
counter alone). Compare the `by_cohort` inclusion rates *relative to each other*
rather than against an absolute target; to confirm the skip path directly,
observe the builder's `BuilderRejected` event with reason
`validity_predicate_not_satisfied`, or run the builder with
`RUST_LOG=payload_builder=trace`.
