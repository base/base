# Load Tests

Load testing and benchmarking framework for Base infrastructure.

## Crate

| Crate | Description |
|-------|-------------|
| `base-load-tests` | Core library with workload generation, transaction submission, and metrics collection |

## Goals

- Provide standardized transaction submission for network load testing
- Centralize workload generation, network orchestration, and metrics collection
- Enable reproducible test scenarios with deterministic configurations

## Quick Start

```bash
# Run load test against local devnet (uses Anvil Account #1)
just load-test devnet

# Run load test against sepolia (requires funded key)
FUNDER_KEY=0x... just load-test sepolia
```

Or run directly with cargo:

```bash
# Build the crate
cargo build -p base-load-tests

# Run tests
cargo test -p base-load-tests

# Run the load test binary with a config file
cargo run -p base-load-tests --bin base-load-test -- path/to/config.yaml
```

## Configuration

All configuration is done via YAML files. See `src/config/test_config.rs` for comprehensive field documentation, or `examples/devnet.yaml` for a working example.

Example minimal config:

```yaml
rpc: http://localhost:8545
block_watcher_url: "ws://localhost:8546"
flashblocks_ws_url: "ws://localhost:7111"
sender_count: 10
target_gps: 2100000
duration: "30s"
```

### Available Configs

| Config | Target | Notes |
|--------|--------|-------|
| `devnet.yaml` | Local devnet | Uses Anvil Account #1 |
| `sepolia.yaml` | Base Sepolia | Requires `FUNDER_KEY` |

### Contract Addresses

Contract addresses for swap testing and related tokens.

#### Base Sepolia (Chain ID: 84532)

| Contract | Address |
|----------|---------|
| Uniswap V3 Router | `0x94cC0AaC535CCDB3C01d6787D6413C739ae12bc4` |
| Aerodrome CL `SwapRouter` | `0x6a786a4f9bc46fF861260545C490a7356c5ecbFe` |
| Load Test Token A (LTTA) | `0x15948C3043A980A8d980d4D615A5E4c9514B0D64` |
| Load Test Token B (LTTB) | `0x4dc9ccF2C5A346c4032B648006B4774Ad2a021c4` |

#### Base Zeronet (Chain ID: 763360)

| Contract | Address |
|----------|---------|
| Uniswap V3 Router | `0x94cC0AaC535CCDB3C01d6787D6413C739ae12bc4` |
| Load Test Token A (LTTA) | `0x27589a9836dd2150036829120f092ad38a0b3740` |
| Load Test Token B (LTTB) | `0xc411b5f78fadab5880a287f21bb7997a192975f3` |

These tokens are deployed via `DeployTestTokenPair.s.sol` and use `FreeTransferERC20` which allows permissionless minting for load testing.

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
