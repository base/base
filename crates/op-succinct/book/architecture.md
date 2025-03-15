# Architecture

## System Overview

OP Succinct transforms any OP Stack chain into a fully validity-proven rollup with a lightweight upgrade that maintains compatibility with existing OP Stack components.

This document explains the standard OP Stack components and the lightweight modifications OP Succinct adds to enable proving blocks with zero-knowledge proofs using [SP1](https://docs.succinct.xyz/docs/sp1/introduction).

## Standard OP Stack Components

Every OP Stack rollup consists of four main components:

- `op-geth`: The execution engine that:
  - Processes user transactions
  - Generates and executes blocks
  - Maintains the L2 state
- `op-batcher`: The transaction bundler that:
  - Collects user transactions
  - Batches them efficiently
  - Submits batches to L1 for data availability
- `op-node`: The derivation engine that:
  - Reads batch data from L1
  - Generates payload attributes
  - Passes payload attributes to `op-geth` to perform state transitions
- `op-proposer`: The state commitment component that:
  - Posts output roots to L1 at regular intervals
  - Capturing the L2 state and enabling withdrawal processing

For more details on these components, refer to the [OP Stack Specification](https://specs.optimism.io/).

## OP Succinct Extensions

OP Succinct enables OP Stack blocks to be proven with SP1 through a lightweight upgrade to op-proposer and the on-chain contract that tracks valid L2 state roots. No changes are needed to `op-geth`, `op-batcher`, or `op-node`.

### Core Components

1. **[Range Program](https://github.com/succinctlabs/op-succinct/tree/main/programs/range/src/main.rs)**
   - Derives and executes batches of blocks
   - Generates proofs of correct execution

2. **[Aggregation Program](https://github.com/succinctlabs/op-succinct/tree/main/programs/aggregation/src/main.rs)**
   - Aggregates multiple range program proofs
   - Reduces on-chain verification costs

3. **OP Succinct L2 Output Oracle**
   - Modified version of the original [L2OutputOracle contract](https://github.com/ethereum-optimism/optimism/blob/3e68cf018d8b9b474e918def32a56d1dbf028d83/packages/contracts-bedrock/src/L1/L2OutputOracle.sol)
   - Stores array of L2 state outputs 
   - Modified to verify ZK proofs to accept output proposals

4. **OP Succinct Proposer**
   - Orchestrates the proving pipeline
   - Monitors L1 for posted batches
   - Manages proof generation and submission using the range and aggregation programs

### Data Flow

![OP Succinct Architecture](./assets/op-succinct-proposer-architecture.jpg)

1. User transactions are processed by standard OP Stack components.
2. The range program generates proofs for batches of transactions.
3. The aggregation program combines these proofs for on-chain verification efficiency.
4. The OP Succinct Proposer submits L2 state outputs to L1 with ZK proofs.
5. The L2 Output Oracle verifies the ZK proofs and accepts valid output proposals.

## Deployment Requirements

To upgrade an existing OP Stack rollup to a fully type-1 ZK rollup using OP Succinct:

1. Deploy the `OPSuccinctL2OutputOracle` contract.
2. Configure and start the OP Succinct Proposer instead of the standard `op-proposer`.

This minimal change set ensures a smooth transition while leveraging the security benefits of ZK proofs.
