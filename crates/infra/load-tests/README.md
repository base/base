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

## Signed validity rollout

Validity workloads default to `validity.signing: adaptive`: requests start
unsigned. An explicit signature-required ingress error switches that endpoint
to signing and retries the same raw transaction with a user-authorized sidecar.
The endpoint state is shared by workers; subsequent requests sign immediately.
Retries remain bounded, transaction hashes and nonces are unchanged, and signed
submissions never silently downgrade to unsigned or plain transactions.

For proactive migration, including nodes in `off` mode or the fleet's
`verify-if-present` stage, configure:

```yaml
validity:
  signing: signed
```

Keep the existing validity ratio and predicate configuration alongside that setting.
Signing uses the controlled sender's key and the EIP-712 contract documented in
[base-execution-txpool](../../execution/txpool/README.md#signed-validity-predicates).
Adaptive mode observes ingress errors only: an asynchronous builder drop cannot
trigger it. In `off` mode, nodes preserve signatures without checking them; operators
should deploy `verify-if-present` fleet-wide to verify signed requests while accepting
legacy clients, and use explicit signing before enabling required enforcement.

## 200ms devnet profile

`examples/denim-devnet.yaml` uses canonical polling at 200ms, without a Flashblocks WebSocket.
It keeps the `devnet.yaml` transaction mix and 20M gas/s target: 4M gas per 200ms block instead
of 40M per 2s block. Configure the devnet separately with Denim active and native building;
use one tenth of the baseline block gas limit to preserve gas/s capacity. The profile does not
change chain configuration.

```bash
# Use a funded local-devnet account, never a production key.
FUNDER_KEY=0x... LOAD_TEST_OUTPUT=denim-results.json \
  cargo run -p base-load-tester-bin --bin base-load-tester -- \
  crates/infra/load-tests/examples/denim-devnet.yaml
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
| `validity-devnet.yaml` | Local devnet | Validity (conditional) workload; routes half the senders through `base_sendRawTransactionValidity`. Run with `FUNDER_KEY=... just load-test run validity-devnet` |
| `real-token-devnet.yaml.template` | Local devnet | Rendered by `just load-test real-token` after deploying the devnet WETH/USDC harness |
| `validity-stress.yaml.template` | Local devnet | Rendered by `just load-test validity-stress` with a freshly deployed `DoubleCounter` |
| `sepolia.yaml` | Base Sepolia | Requires `FUNDER_KEY` |
| `real-token-sepolia.yaml` | Base Sepolia | Uses predeployed WETH/USDC and the Uniswap V3 swap router; run with `just load-test real-token sepolia`; recover with `just load-test real-token-recover sepolia` |
| `real-token-mainnet-snapshot.yaml` | Local/shadow Base mainnet snapshot | Wraps funded ETH into WETH, acquires USDC, then runs random-direction Uniswap V3 and Aerodrome CL swaps; run with `just load-test real-token mainnet-snapshot` |
| `mainnet-mix-mainnet-snapshot.yaml` | `base-bench snapshot` on Base mainnet | Transaction mix and gas/s of a sampled mainnet week; regenerate with `etc/scripts/local/mainnet-mix.py`, whose docstring lists the mapping and its limits |
| `mainnet-mix-eip8430-mainnet-snapshot.yaml` | Everest-active target | Opt-in synthetic variant with the 41/1000 ETH-transfer weight split into 21 regular and 20 native EIP-8430 transfers; not a measured native tx share. The sampling generator emits only the ordinary baseline |
| `eip8430-transfer.yaml` | Everest-active target | Native EOA self-pay ETH transfers; protocol nonce lane, one call per transaction |
| `zeronet.yaml` | Base Zeronet | Requires `FUNDER_KEY` |

EIP-8430 examples require Everest activation and native transaction support on the target.

`eip8430-swap-mainnet-snapshot.yaml` exercises app-style token swaps on an
Everest-active local mainnet snapshot. The existing `uniswap_v3` workload accepts
`native: true` (default: `false`): each native transaction contains one phase with
an ERC20 approval for the exact input amount followed by the swap. It uses the
existing random-direction amounts and real-token setup. The declared gas ceiling
is 500,000; calibration measures the native batch receipt for pacing.

Set `sender_count: 1` for a single actor, or `sender_count: 16` for actors
contending on the same pool. All actors use the configured router and token pair.
The existing setup wraps WETH, acquires the pair token and approves the router;
the measured native batch still explicitly sets its exact allowance before each
swap. Token inventories can run out during sustained random-direction traffic.
These are synthetic load swaps with zero minimum output, not trading strategies.

Native and ordinary swaps cannot share a router/token allowance in one run:
native batches consume their exact approvals, while ordinary swaps rely on setup
allowances. Configuration rejects overlapping positive-weight routes.

Native swaps use `eth_sendRawTransaction` and cannot be combined with validity
sidecars. They do not implement storage-predicate limit orders or timestamp
windows. No contracts are deployed by this profile; use only a local snapshot
with the configured contracts and test funds, never a public RPC.

The transfer profiles (`eip8430-transfer.yaml` and the synthetic mainnet mix)
have the following properties.
They use the existing `Eip8130Signed` consensus implementation (type `0x79`), ECDSA EOA
authentication, self-payment, and nonce key zero, sharing each sender's protocol nonce
stream with ordinary transactions. Calibration submits the same native envelope as measured
load and uses its receipt gas; the 200,000 gas limit is a ceiling, not a gas estimate.
Fresh recipients are supported through `fresh_recipient_ratio`. Validity submission is
not supported with native transfers. These examples do not exercise configured-account
authentication, sponsored payers, nonce-free transactions, or multiple calls/phases.
The synthetic mix retains the baseline gas/s target and stand-in limitations; its gas/tx
must be measured again and must not be inferred from the original ordinary-transfer estimate.

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

#### x402 settlement testing

`type: x402` submits the x402 exact-scheme settlement, a payer-signed USDC
`transferWithAuthorization`, from a separate gas-paying facilitator. It requires a
Base mainnet snapshot, at least three accounts, and the existing `real_token_setup`
WETH-to-USDC acquisition alongside a swap workload. Setup checks the real proxy's
domain separator, every payer's `balanceOf`, and `isBlacklisted`; no token storage
is overwritten. Each facilitator uses the preceding funded account as payer, and
the runner's following recipient as merchant. Random authorization nonces survive
calibration generator reconstruction. Windows run from one second before setup's
snapshot timestamp until seven snapshot days later; longer runs must restart setup.
Each payment transfers one micro-USDC; other mixed token workloads can still exhaust shared balances.

The mainnet mix generator classifies each transaction emitting canonical USDC
`AuthorizationUsed` once, before other classes. This is an upper-bound x402 proxy:
EIP-3009 has other users, and transactions may settle more than one authorization.
It does not attribute facilitators. The prior sample's relative workload weights are preserved,
scaled to 978 of 1,000, with x402 inserted at its measured upper-bound share of 22 (2.2%).
Runtime pacing still calibrates actual receipt gas.
Benchmark mainnet-mix runs selecting `mainnet-mix-mainnet-snapshot.yaml` after this
change are not comparable with runs using the earlier mix.

#### Batched token transfers

`type: token_move` sends `transfers_per_tx` (default 25) one-unit transfers of the
`real_token_setup` pair token in one
[Disperse](https://disperse.app) `disperseTokenSimple` call, which pulls from
`msg.sender`. It requires the existing `real_token_setup` alongside a swap workload,
and Disperse deployed at `0xD152f549545093347A162Dce210e7293f1452150` (Base mainnet
snapshots); setup approves Disperse like a swap router. The first recipient is the
runner recipient, so `fresh_recipient_ratio` applies to it; the rest are random
senders. With real USDC on a Base mainnet fork, 25 transfers to funded recipients
use about 332k gas. `transfers_per_tx` is capped at 477 so the gas limit fits the
2^24 per-tx cap (EIP-7825, enabled with Azul).

#### ERC-4337 bundle stand-in

`type: aa_bundle` sends one `handleOps` transaction carrying `ops_per_bundle` (default 12)
ops. Per op, the contract recovers an ECDSA signature over the op, increments the signer's
nonce slot, credits a fresh recipient slot, and emits one event; `padding_bytes` (default 0)
appends random calldata. Setup deploys the contract through the CREATE2 deployment proxy
(`0x4e59…956C`) unless it already exists. On a Base mainnet fork each op costs ~36k gas over a
~29k base: the default sends ~457k gas, 2,436 calldata bytes and 12 logs per transaction,
within mainnet's ERC-4337 bundles (390–530k gas, ~2.6 KB, ~12 logs). Each bundle's gas limit
(`40_000 + 60_000 * ops_per_bundle + 40 * padding_bytes`) must fit the EIP-7825 per-tx cap of
16,777,216 gas: at most 278 ops without padding, or 416,930 padding bytes with one op.


#### Storage writes

`type: storage` writes `slots_per_tx` slots per transaction through `StorageFiller`, a
`new_slot_ratio` fraction of them fresh. Without `contract`, setup deploys `StorageFiller`
through the CREATE2 deployment proxy (`0x4e59…956C`) unless it already exists.

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
supply, during the load phase each sender transfers its own token to its pair partner (alice <-> bob;
odd sender counts get one extra funded account so nobody self-transfers), and during teardown each
sender burns its remaining balance. A fresh per-run salt keeps each run's token addresses distinct.

Requires Beryl activation (B-20 factory and token features must be active on the target chain).

```yaml
# Each sender creates its own B-20 token and transfers it to its pair partner
transactions:
  - weight: 100
    type: b20
```

#### Swap Testing

Swap payloads randomly choose direction for each generated transaction, alternating between `token_in → token_out` and `token_out → token_in`.

`real_token_setup` runs a pre-test phase before the measured loop: it wraps sender ETH into WETH, acquires the paired token through the configured acquisition route if the sender's balance is below `amount_per_sender`, and approves all measured routers for both tokens. When present and enabled, it replaces fixture-token minting (`swap_token_amount`).

When this config is executed via `base-bench local --workload-config`, setting
`deploy_devnet_swap_harness: true` on the workload entry deploys a fresh
devnet USDC token plus Uniswap/Aerodrome router shims and auto-wires all swap
and acquisition addresses. In that mode, these address fields can be omitted.

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

On `uniswap_v3`, `reverse_amounts: auto` instead derives the reverse range before the run: it reads `sqrtPriceX96` from the router factory's pool for `token_in`/`token_out`/`fee` and converts `min_amount` and `max_amount` into `token_out` raw units at that price, so both directions move equal value and sustained runs do not drain one token. It cannot be combined with `reverse_min_amount` or `reverse_max_amount`, and fails when the pool does not exist or an amount converts to zero. The derived range is logged at info.

```yaml
  - weight: 100
    type: uniswap_v3
    # router, token_in (WETH), token_out (USDC) and fee as above.
    min_amount: "10000000000000" # WETH wei
    max_amount: "100000000000000"
    reverse_amounts: auto # USDC raw units at the pool price
```

`via` on `uniswap_v3` routes each swap through intermediate tokens with `SwapRouter02` `exactInput`; each entry names a token and the fee of the pool that swaps into it, and `fee` stays the last pool's fee. Reverse swaps take the same pools backwards. Only the router pulls intermediate tokens, so senders need no extra balances or approvals. `reverse_amounts: auto` prices the range through every pool in turn. `via` cannot be combined with `native`, and the gas limit is 250,000 per pool. These Base mainnet routes end at USDC, so they work with the `real_token_setup` above:

```yaml
  - weight: 100
    type: uniswap_v3
    router: "0x2626664c2603336E57B271c5C0b26F421741e481"
    token_in: "0x4200000000000000000000000000000000000006" # WETH
    token_out: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" # USDC, gitleaks:allow
    via:
      - token: "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf" # cbBTC, gitleaks:allow
        fee: 500
    fee: 500
    min_amount: "10000000000000"
    max_amount: "100000000000000"
    reverse_amounts: auto
  # Three pools: WETH -> DAI -> USDbC -> USDC.
  #   via:
  #     - { token: "0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb", fee: 500 } # DAI, gitleaks:allow
  #     - { token: "0xd9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA", fee: 100 } # USDbC, gitleaks:allow
  #   fee: 100
```

#### Uniswap V3 liquidity testing

`type: uniswap_v3_liquidity` sends Uniswap V3 `NonfungiblePositionManager` transactions on a real pool and requires `real_token_setup` for the pool's two tokens. Setup reads the pool's current tick, fixes a range of 100 tick spacings on each side, and mints each sender one position holding 64 increases' worth of liquidity. The run then mixes, by `ops` weight: `mint` (a new position, new storage), `increase` (adds `amount0`/`amount1` to the sender's setup position), `decrease` (removes a quarter of an increase's worth), `collect` (owed tokens back to the sender), and `multicall` (all four in one transaction). `decrease` may not outweigh `increase`, so setup positions do not empty even if the price drifts to a range edge of a standard fee tier. `amount0` and `amount1` cap the tokens added per mint or increase, and the pool takes them in its current ratio. Mints and the undecreased three quarters of each increase stay in positions, so size `real_token_setup` balances for the run's mint and increase count.

```yaml
  - weight: 100
    type: uniswap_v3_liquidity
    position_manager: "0x03a520b32C04BF3bEEf7BEb72E919cf822Ed34f1" # Base mainnet
    token0: "0x4200000000000000000000000000000000000006" # WETH; token0 must sort below token1
    token1: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" # USDC, public token address: gitleaks:allow
    fee: 500
    amount0: "10000000000000" # WETH wei
    amount1: "23000" # USDC raw units
    ops: {mint: 1, increase: 1, decrease: 1, collect: 1} # default; listed ops only, others 0
```

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
port 8545. Ingress must have forwarding configured as described below.

A configurable fraction of *senders* can route their entire traffic through the
`base_sendRawTransactionValidity` endpoint, attaching validity predicates to
every transaction they submit. All five server predicate types are supported:
the state-based `balance`, `nonce`, and `storage` conditions, and the build-position
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
    - type: nonce
      address: sender
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
      op: "<="                  # required block-number expiry bound
      offset: "10"
    - type: flashblock_index
      op: ">="
      value: "1"
```

The `nonce` predicate compares the watched account's protocol nonce immediately
before transaction execution, using the same operators and `address`, `op`, and
`value` fields as `balance`. Absent accounts have nonce zero. It does not read
EIP-8130 channel nonces; use a storage predicate for those. Like every validity
submission, nonce predicates must be paired with a `block_number` upper bound.

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

**Required setup for end-to-end evaluation.** If forwarding is used, configure `--enable-tx-forwarding` and `--builder-rpc-urls=<url>` on ingress.
The submission proxy must also route `base_sendRawTransactionValidity` to that ingress.
Rejected requests must not be retried as plain `eth_sendRawTransaction`.
Both build paths evaluate state and block predicates. `flashblock_index` requires the
flashblocks path; the native/Denim builder permanently rejects that predicate.
3. The builder runs the flashblocks build path (the only builder path wired in
   the shipped binaries), which is where predicates are evaluated against state.

If `validity.ratio > 0` but the ingress endpoint does not serve
`base_sendRawTransactionValidity`, the run fails loudly at startup rather than
silently degrading to plain submission.

**Interpreting the results.** Signature-rejection metrics describe admission policy,
not unsatisfied predicates. A transaction whose predicate is false is skipped by the builder and
simply never confirms (it is not distinguishable from an ordinary drop by a
counter alone). Compare the `by_cohort` inclusion rates *relative to each other*
rather than against an absolute target; to confirm the skip path directly,
observe the builder's `BuilderRejected` event with reason
`validity_predicate_not_satisfied`, or run the builder with
`RUST_LOG=payload_builder=trace`.
