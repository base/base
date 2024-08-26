# `op-succinct` Tutorial

This tutorial guides you through upgrading an existing OP Stack chain to a ZK-OP Stack chain. It assumes you already have a running OP Stack chain. If not, please follow [Optimism's tutorial](https://docs.optimism.io/builders/chain-operators/tutorials/create-l2-rollup) first.

## Prerequisites

Before starting, ensure you have:

1. Access to the Admin keys for upgrading L1 contracts of the OP Stack chain.
2. Fault proofs turned off (you should be using `L2OutputOracle`, not `FaultDisputeGameFactory` for output roots).
3. Stopped the original `op-proposer`.
4. At least 1 ETH in your `PROPOSER` wallet.
5. All dependencies from the Optimism tutorial installed.
6. Your L2 geth node running with `--gcmode=archive` and `--state.scheme=hash` flags. You can do this with [ops-anton](https://github.com/anton-rs/ops-anton/blob/main/L2/op-mainnet/op-geth/op-geth.sh).

## Overview

At a high level, the `op-succinct` upgrade focuses on replacing the `op-proposer` component and its associated contracts. This change allows the chain to progress only with ZK-proven blocks, while keeping the other components (`op-geth`, `op-batcher`, and `op-node`) unchanged.

When an OP Stack chain is running, there are 4 main components:
1. `op-geth` (Sequencer mode): Takes transactions from users and uses them to generate blocks and execute blocks.
2. `op-batcher`: Batches transactions from users and submits them to the L1.
3. `op-node`: Reads batch data from L1 and uses it to drive `op-geth` in non-sequencer mode to perform state transitions.
4. `op-proposer`: Posts an output root to L1 at regular intervals, which captures the L2 state so withdrawals can be processed.

## Step-by-Step Guide

### Step 1: Setup

1. Clone the `op-succinct` repo:
   ```
   git clone https://github.com/succinctlabs/op-succinct.git
   ```

2. Create and fill in the environment files:
   ```
   cp .env.example .env
   cp .env.server.example .env.server
   ```

3. Fill in the `contracts/zkconfig.json` file with your chain's specific details.

    <details>
    <summary>Field Info</summary>

    - `startingBlockNumber`: The L2 block number at which the rollup starts. Default should be 0.
    - `l2RollupNode`: The URL of the L2 rollup node. (After the tutorial, this is `http://localhost:8545`)
    - `submissionInterval`: The number of L2 blocks between each L1 output submission.
    - `l2BlockTime`: The time in seconds between each L2 block.
    - `proposer`: The Ethereum address of the proposer account. If `address(0)`, anyone can submit proofs.
    - `challenger`: The Ethereum address of the challenger account. If `address(0)`, no one can dispute proofs.
    - `finalizationPeriod`: The time period (in seconds) after which a proposed output becomes finalized. Specifically, the time period after
    which you can withdraw your funds against the proposed output.
    - `chainId`: The chain ID of the L2 network.
    - `owner`: The Ethereum address of the `ZKL2OutputOracle` owner, who can update the verification key and verifier address.
    - `vkey`: The verification key for the aggregate program. Run `cargo run --bin vkey --release` to generate this.
    - `verifierGateway`: The address of the verifier gateway contract.
    - `l2OutputOracleProxy`: The address of your OP Stack chain's L2 Output Oracle proxy contract which will be upgraded.

    </details>

### Step 2: Deploy ZKL2OutputOracle

1. Navigate to the contracts directory:
   ```
   cd contracts
   ```

2. Run the upgrade script:
   ```
   forge script script/ZKUpgrader.s.sol:ZKUpgrader --rpc-url <L1 RPC> --private-key <ADMIN PK> --verify --verifier etherscan --etherscan-api-key <ETHERSCAN API> --broadcast --slow --vvvv
   ```
   Alternatively, use:
   ```
   just upgrade-l2oo <L1_RPC> <ADMIN_PK> <ETHERSCAN_API_KEY>
   ```

To run your OP Stack chain with ZK proofs you need to replace your chain's `L2OutputOracle` with `ZKL2OutputOracle`. 

The existing `L2OutputOracle` allows a permissioned `proposer` role to submit output roots. The permissioned `challenger` role can dispute these output roots. There are no checks on the validity of these claims.

The ZKL2OutputOracle makes the following changes:
- Require a valid SP1 proof for each output root submission.
- If `proposer == address(0)`, anyone can submit a proof.

### Step 3: Launch `op-succinct`

1. Build and start the Docker container:
   ```
   docker compose build
   docker compose up -d
   ```

This launches a Docker container with `op-proposer` from a fork of the `optimism` monorepo which can be found [here](https://github.com/succinctlabs/optimism/tree/zk-proposer) as well as a server that generates ZK proofs using Kona (Optimism's state transition function library) and SP1 (a zkVM).

The modified `op-proposer` performs the following tasks:
- Monitors L1 state to determine when to request a proof.
- Requests proofs from the Kona SP1 server.
- Once proofs have been generated for a sufficiently large range (specified by `SUBMISSION_INTERVAL` in `zkconfig.json`), aggregates batch proofs and submits them on-chain.

## Verification

After completing these steps, your chain will be running as a ZK-OP chain:

- The L1 contract (ZKL2OutputOracle) verifies ZK proofs.
- The Kona SP1 server generates ZK proofs.
- The modified `op-proposer` submits ZK-proven output roots to L1.

🎉 Congratulations! 🎉 You've successfully upgraded to a ZK-OP chain with `op-succinct`.
