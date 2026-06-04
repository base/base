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

All configuration is done via YAML files. See `src/config/test_config.rs` for comprehensive field documentation, or `examples/devnet.yaml` for a working example.
Example minimal config:

```yaml
transaction_submission_rpcs:
  - "http://localhost:8545"
# Add more URLs to shard submit batches across multiple HTTP endpoints.
query_rpc: "http://localhost:8545"
# Optional: clear pending transactions from these admin RPC nodes for all sender addresses.
txpool_nodes: []
flashblocks_ws: "ws://localhost:7111"
sender_count: 10
target_gps: 2100000
duration: "30s"
```

`flashblocks_ws` is required for builder flashblocks broadcast latency data.
`transaction_submission_rpcs` accepts either a single URL string or a list; submit batches are
distributed across the configured HTTP endpoints.
`txpool_nodes` is optional and defaults to an empty list; when present, the load tester calls
`admin_dropSenderTransactions` for every sender address on every configured node before funding.
Canonical confirmation and gas metrics are collected by polling `query_rpc` for new blocks and
fetching `eth_getBlockReceipts` for each observed block, so `query_rpc` must support
`eth_getBlockReceipts`.

### Available Configs

| Config | Target | Notes |
|--------|--------|-------|
| `devnet.yaml` | Local devnet | Uses Anvil Account #1 |
| `real-token-devnet.yaml.template` | Local devnet | Rendered by `just load-test real-token` after deploying the devnet WETH/USDC harness |
| `sepolia.yaml` | Base Sepolia | Requires `FUNDER_KEY` |
| `real-token-sepolia.yaml` | Base Sepolia | Uses predeployed WETH/USDC and the Uniswap V3 swap router; run with `just load-test real-token sepolia`; recover with `just load-test real-token-recover sepolia` |
| `real-token-mainnet-snapshot.yaml` | Local/shadow Base mainnet snapshot | Wraps funded ETH into WETH, acquires USDC, then runs random-direction Uniswap V3 and Aerodrome CL swaps; run with `just load-test real-token mainnet-snapshot` |

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
The load tester handles the full lifecycle: token creation via the B-20 factory, role grants
(`MINT_ROLE` / `BURN_ROLE` to every sender), minting during setup, and burning during teardown.

Requires Beryl activation (B-20 factory and token features must be active on the target chain).

```yaml
# Auto-create a new B-20 token per run (devnet/zeronet)
transactions:
  - weight: 100
    type: b20

# Use a pre-deployed B-20 token
transactions:
  - weight: 100
    type: b20
    contract: "0x..."
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
