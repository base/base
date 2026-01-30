# Cost Estimation Tools

This guide covers the scripts for estimating proving costs and testing execution: `multi` and `cost-estimator`.

## Setup

Before running these scripts, set up a `.env` file in the project root:

```bash
L1_RPC=<YOUR_L1_RPC_ENDPOINT>
L2_RPC=<YOUR_L2_RPC_ENDPOINT>
L2_NODE_RPC=<YOUR_L2_NODE_RPC_ENDPOINT>
```

## Multi Script

The `multi` script executes the OP Succinct range proof program for a block range. Use it to test proof generation or generate actual proofs.

### Usage

```bash
# Execute without proving (generates execution report)
cargo run --bin multi -- --start 1000 --end 1020

# Generate compressed proofs
cargo run --bin multi -- --start 1000 --end 1020 --prove
```

### Output

- **Execution mode**: Prints execution stats and saves to `execution-reports/multi/{chain_id}/{start}-{end}.csv`
- **Prove mode**: Saves proof to `data/{chain_id}/proofs/{start}-{end}.bin`

## Cost Estimator

The `cost-estimator` estimates proving costs without generating proofs. It splits large ranges into batches and runs them in parallel.

### Usage

```bash
cargo run --bin cost-estimator -- \
    --start 2000000 \
    --end 2001800 \
    --batch-size 300
```

For best estimation, use a range bigger than the batcher interval with batch size equal to the range.

### Output

Execution report saved to `execution-reports/{chain_id}/{start}-{end}-report.csv` with metrics:
- Total instruction count
- Oracle verification / derivation / block execution costs
- SP1 gas usage
- Transaction counts and EVM gas
- Precompile cycles (BN pair, add, mul, KZG eval, etc.)

## Witness Caching

Both scripts support witness caching to skip the time-consuming witness generation step on subsequent runs.

### Why Cache?

The proving pipeline has two stages:

```
host.run() → WitnessData → get_sp1_stdin() → SP1Stdin
   [hours]                    [milliseconds]
```

Witness generation (`host.run()`) fetches L1/L2 data and executes blocks, which can take **hours** for large ranges. Caching saves this data to disk.

We cache `SP1Stdin` because:
1. It skips the hours-long `host.run()` bottleneck
2. SP1Stdin implements serde serialization via bincode
3. The cache is compatible across Ethereum and Celestia DA (both use the same witness format)

Note: The `get_sp1_stdin()` conversion is milliseconds, so caching after this step has negligible overhead.

### Cache Flag

Use `--cache` to enable caching. If a cache file exists for the block range, it will be loaded. Otherwise, witness generation runs and the result is saved to cache.

### Examples

```bash
# First run: generates witness and saves to cache
cargo run --bin multi -- --start 1000 --end 1020 --cache

# Second run: loads from cache (instant), then proves
cargo run --bin multi -- --start 1000 --end 1020 --cache --prove

# Force regenerate by deleting cache first
rm data/{chain_id}/witness-cache/1000-1020-stdin.bin
cargo run --bin multi -- --start 1000 --end 1020 --cache

# Cost estimator with caching
cargo run --bin cost-estimator -- --start 1000 --end 1100 --batch-size 10 --cache
```

### Cache Location

```
data/{chain_id}/witness-cache/{start_block}-{end_block}-stdin.bin
```

Example: `data/8453/witness-cache/1000-1020-stdin.bin` for Base.

### DA Compatibility

| DA Type | Compatible With |
|---------|-----------------|
| Ethereum (default) | Celestia |
| Celestia | Ethereum |
| EigenDA | EigenDA only |

Cache files are compatible between Ethereum and Celestia (both use `DefaultWitnessData`), but **not** with EigenDA (uses `EigenDAWitnessData`). Don't mix cache files across incompatible DA types.

### Cache Management

```bash
# Clear all cache for a chain
rm -rf data/{chain_id}/witness-cache/

# Clear specific range
rm data/{chain_id}/witness-cache/{start}-{end}-stdin.bin
```

Cache files are typically 100MB-1GB per range.
