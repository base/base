# Prove Scripts

The prove scripts generate range and aggregation proofs for OP Succinct. By default they use the [Succinct Prover Network](https://docs.succinct.xyz/docs/sp1/prover-network/intro); for self-hosted proving, see [Self-Hosted Proving Cluster](./self-hosted-cluster.md).

## Overview

OP Succinct uses a two-tier proving architecture:

1. **Range Proofs** (`multi`): Generate compressed proofs for a range of L2 blocks.

2. **Aggregation Proofs** (`agg`): Combine multiple range proofs into a single aggregation proof, reducing on-chain verification costs.

> **Note:** All range proofs must be in compressed mode for aggregation. The prove scripts handle this automatically.

For cost estimation without proving, see [Cost Estimation Tools](./cost-estimation-tools.md).

## Setup

### Environment Configuration

Create a `.env` file in the project root directory:

```bash
# RPC Endpoints
L1_RPC=<YOUR_L1_RPC_ENDPOINT>
L1_BEACON_RPC=<YOUR_L1_BEACON_RPC_ENDPOINT>
L2_RPC=<YOUR_L2_RPC_ENDPOINT>
L2_NODE_RPC=<YOUR_L2_NODE_RPC_ENDPOINT>

# Network Prover Configuration
NETWORK_PRIVATE_KEY=<YOUR_NETWORK_PRIVATE_KEY>

# Proof Strategy Configuration
RANGE_PROOF_STRATEGY=reserved    # Options: reserved, hosted, auction
AGG_PROOF_STRATEGY=reserved      # Options: reserved, hosted, auction
AGG_PROOF_MODE=plonk             # Options: plonk, groth16
```

### Environment Variables

#### Required

| Variable | Description |
|----------|-------------|
| `L1_RPC` | L1 Archive Node endpoint |
| `L1_BEACON_RPC` | L1 Consensus (Beacon) Node endpoint |
| `L2_RPC` | L2 Execution Node (`op-geth`) endpoint |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`) endpoint |

#### Required (proving only)

| Variable | Description |
|----------|-------------|
| `NETWORK_PRIVATE_KEY` | Required when using `--prove` with the Succinct Prover Network. See the [Succinct Prover Network Quickstart](https://docs.succinct.xyz/docs/sp1/prover-network/quickstart) for setup instructions. Not needed for execute-only runs or cluster mode. |

#### Optional (`multi` script)

| Variable | Description | Default |
|----------|-------------|---------|
| `RANGE_PROOF_STRATEGY` | Proof fulfillment strategy for range proofs | `reserved` |
| `USE_KMS_REQUESTER` | Use AWS KMS for network signing (`NETWORK_PRIVATE_KEY` becomes a KMS key ARN) | `false` |

#### Optional (`agg` script)

| Variable | Description | Default |
|----------|-------------|---------|
| `AGG_PROOF_STRATEGY` | Proof fulfillment strategy for aggregation proofs | `reserved` |
| `AGG_PROOF_MODE` | Proof mode for aggregation proofs (`plonk` or `groth16`) | `plonk` |
| `USE_KMS_REQUESTER` | Use AWS KMS for network signing (`NETWORK_PRIVATE_KEY` becomes a KMS key ARN) | `false` |

Each script reads only its own strategy env var, so `RANGE_PROOF_STRATEGY` and `AGG_PROOF_STRATEGY` can be set independently.

**Proof Strategies:**
- `reserved`: Uses reserved SP1 network capacity
- `hosted`: Uses hosted proof generation service
- `auction`: Uses auction-based proof fulfillment

**Proof Modes:**
- `plonk`: PLONK proof system (default)
- `groth16`: Groth16 proof system

### Getting Started with the Prover Network

1. Follow the [Succinct Prover Network Quickstart](https://docs.succinct.xyz/docs/sp1/prover-network/quickstart) to set up your account and obtain a private key.

2. Set the `NETWORK_PRIVATE_KEY` environment variable:
   ```bash
   NETWORK_PRIVATE_KEY=0x...
   ```

3. Run the prove scripts. The binaries will automatically use the network prover with your configured key.

## Generating Range Proofs

The `multi` binary generates compressed range proofs for a specified block range.

### Usage

```bash
cargo run --bin multi --release -- \
    --start <START_BLOCK> \
    --end <END_BLOCK> \
    --prove
```

### Example

```bash
# Generate a compressed range proof for blocks 1000-1300
cargo run --bin multi --release -- \
    --start 1000 \
    --end 1300 \
    --prove
```

### Output

Range proofs are saved to `data/{chain_id}/proofs/range/{start_block}-{end_block}.bin`.

## Generating Aggregation Proofs

The `agg` binary aggregates multiple compressed range proofs into a single aggregation proof.

### Usage

```bash
cargo run --bin agg --release -- \
    --proofs <PROOF_1>,<PROOF_2>,<PROOF_N> \
    --prover <PROVER_ADDRESS> \
    --prove
```

### Example

```bash
# Aggregate three consecutive range proofs covering blocks 1000-1900
cargo run --bin agg --release -- \
    --proofs 1000-1300,1300-1600,1600-1900 \
    --prover 0x1234567890abcdef1234567890abcdef12345678 \
    --prove
```

### Parameters

| Parameter | Description | Required |
|-----------|-------------|----------|
| `--proofs` | Comma-separated list of proof names (without `.bin` extension) | Yes |
| `--prover` | Prover wallet address included in the aggregation proof | Yes |
| `--prove` | Generate proof (omit to only execute and verify inputs) | No |
| `--env-file` | Path to environment file (default: `.env`) | No |
| `--cluster-timeout` | Proving timeout in seconds (cluster mode only) | No (default: 21600) |

### Requirements

- Proof files must exist in `data/{chain_id}/proofs/range/` directory
- Proof names should match the range format: `{start_block}-{end_block}`
- Range proofs must be consecutive (e.g., 1000-1300, 1300-1600, 1600-1900)

### Output

Aggregation proofs are saved to `data/{chain_id}/proofs/agg/{proof_names}.bin`.

## End-to-End Workflow

All proofs are stored under `data/{chain_id}/proofs/` with consistent hyphen-separated naming (`{start}-{end}.bin`).

### Local Proving

#### 1. Generate Range Proofs

Run `multi --prove` for each block range. Proofs are saved to `data/{chain_id}/proofs/range/{start}-{end}.bin`.

```bash
cargo run --bin multi --release -- --start 1000 --end 1300 --prove
cargo run --bin multi --release -- --start 1300 --end 1600 --prove
cargo run --bin multi --release -- --start 1600 --end 1900 --prove
```

#### 2. Aggregate Proofs

Run `agg --prove` with the proof names (without `.bin` extension) matching files in `data/{chain_id}/proofs/range/`.

```bash
cargo run --bin agg --release -- \
    --proofs 1000-1300,1300-1600,1600-1900 \
    --prover 0x1234567890abcdef1234567890abcdef12345678 \
    --prove
```

The aggregation proof is saved to `data/{chain_id}/proofs/agg/`.

### Network Fetch

If proofs were generated via the SP1 network, use `fetch_and_save_proof` to download them into the same `data/{chain_id}/proofs/range/` directory.

#### 1. Generate Range Proofs (network)

```bash
cargo run --bin multi --release -- --start 1000 --end 1300 --prove
```

#### 2. Fetch Proofs from the Network

```bash
cargo run --bin fetch-and-save-proof --release -- \
    --request-id <REQUEST_ID> --chain-id <CHAIN_ID> --start 1000 --end 1300
```

This saves the proof as `data/{chain_id}/proofs/range/1000-1300.bin`. Repeat for each range proof.

#### 3. Aggregate Proofs

```bash
cargo run --bin agg --release -- \
    --proofs 1000-1300,1300-1600,1600-1900 \
    --prover 0x1234567890abcdef1234567890abcdef12345678 \
    --prove
```

## Witness Caching

Witness generation (`host.run()`) fetches L1/L2 data and executes blocks, which can take **hours** for large ranges. Caching saves the generated witness to disk so subsequent runs skip this step.

```
host.run() → WitnessData → get_sp1_stdin() → SP1Stdin
   [hours]                    [milliseconds]
```

### Usage

Use `--cache` to enable caching. On the first run, the witness is generated and saved. On subsequent runs, the cached witness is loaded instantly.

```bash
# First run: generates witness and saves to cache
cargo run --bin multi --release -- --start 1000 --end 1020 --cache

# Second run: loads from cache (instant), then proves
cargo run --bin multi --release -- --start 1000 --end 1020 --cache --prove
```

### Cache Location

```
data/{chain_id}/witness-cache/{start_block}-{end_block}-stdin.bin
```

### DA Compatibility

| DA Type | Compatible With |
|---------|-----------------|
| Ethereum (default) | Celestia |
| Celestia | Ethereum |
| EigenDA | EigenDA only |

Cache files are compatible between Ethereum and Celestia, but **not** with EigenDA. Don't mix cache files across incompatible DA types.

### Cache Management

```bash
# Clear all cache for a chain
rm -rf data/{chain_id}/witness-cache/

# Clear specific range
rm data/{chain_id}/witness-cache/{start}-{end}-stdin.bin
```

Cache files are typically 100MB-1GB per range.

## Local Development

For testing without incurring proving costs, omit the `--prove` flag:

```bash
# Execute range proof program without proving
cargo run --bin multi --release -- \
    --start 1000 \
    --end 1300

# Execute aggregation program without proving
cargo run --bin agg --release -- \
    --proofs 1000-1300,1300-1600 \
    --prover 0x1234567890abcdef1234567890abcdef12345678
```

This runs execution and reports cycle counts without submitting proof requests to the network. No `NETWORK_PRIVATE_KEY` or proof strategy configuration is needed for execute-only runs.
