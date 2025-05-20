# Cost Estimator

The Cost Estimator is a powerful tool that allows you to estimate proving costs for op-succinct without actually generating proofs. This helps you **optimize your proof generation strategy** by identifying potential causes of unexpected costs.

## Overview

The cost estimator simulates the proof generation process and provides detailed metrics including:
- Total instruction count
- Oracle verification costs
- Derivation costs
- Block execution costs
- Blob verification costs
- **SP1 gas usage**
- Transaction counts and EVM gas usage
- Various precompile cycles (BN pair, add, mul, KZG eval, etc.)

## Setup

Before running the cost estimator, you need to set up a .env file in the project root directory with the following RPC endpoints:

```bash
L1_RPC=<YOUR_L1_RPC_ENDPOINT>
L1_BEACON_RPC=<YOUR_L1_BEACON_RPC_ENDPOINT>
L2_RPC=<YOUR_L2_RPC_ENDPOINT>
L2_NODE_RPC=<YOUR_L2_NODE_RPC_ENDPOINT>
```

## Usage

To run the cost estimator, use the following command:

```bash
RUST_LOG=info cargo run --bin cost-estimator -- \
    --start <START_BLOCK> \
    --end <END_BLOCK> \
    --batch-size <BATCH_SIZE>
```

Example:

```bash
RUST_LOG=info cargo run --bin cost-estimator -- \
    --start 2000000 \
    --end 2001800 \
    --batch-size 1800
```

<br>

For the best estimation, use a range bigger than the batcher interval with the batch size equal to the range.

### Output

The execution report is saved in the `execution-reports/{chain_id}/{start_block}-{end_block}-report.csv` directory.
