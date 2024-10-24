# Proposer

Now that you have deployed the `OPSuccinctL2OutputOracle` contract, you can start the `op-succinct` service which replaces the normal `op-proposer` service in the OP Stack.

The `op-succinct` service consists of two containers:
- `op-succinct-server`: Receives proof requests from the `op-succinct-proposer`, generates the witness for the proof, and submits the proof to the Succinct Prover Network. Handles the communication with the [Succinct's Prover Network](https://docs.succinct.xyz/generating-proofs/prover-network) to fetch the proof status and completed proof data.
- `op-succinct-proposer`: Monitors L1 state to determine when to request a proof. Sends proof requests to the `op-succinct-server`. Once proofs have been generated for a sufficiently large range, aggregates range proofs into an aggregation proof. Submits the aggregation proof to the `OPSuccinctL2OutputOracle` contract which includes the L2 state outputs.

We've packaged the `op-succinct` service in a docker compose file to make it easier to run.

## Prerequisites

### RPC Requirements

Confirm that your RPC's have all of the required endpoints. More details can be found in the [prerequisites](./prerequisites.md#requirements) section.

### Hardware Requirements

We recommend the following hardware configuration for the `op-succinct` service containers:

Using the docker compose file:

- `op-succinct`: 16 vCPUs, 16GB RAM

Running as separate containers:

- `op-succinct-server`: 16 vCPUs, 16GB RAM
- `op-succinct-proposer`: 1 vCPU, 4GB RAM

For advanced configurations, depending on the number of concurrent requests you expect, you may need to increase the number of vCPUs and memory allocated to the `op-succinct-server` container.

## Environment Setup

Before starting the proposer, the following environment variables should be in your `.env` file. You should have already set up your environment when you deployed the L2 Output Oracle. If you have not done so, follow the steps in the [L2 Output Oracle](./l2-output-oracle.md) section.

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L1_BEACON_RPC` | L1 Consensus (Beacon) Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |
| `PROVER_NETWORK_RPC` | Default: `rpc.succinct.xyz`. |
| `SP1_PRIVATE_KEY` | Key for the Succinct Prover Network. Get access [here](https://docs.succinct.xyz/generating-proofs/prover-network). |
| `SP1_PROVER` | Default: `network`. Set to `network` to use the Succinct Prover Network. |
| `PRIVATE_KEY` | Private key for the account that will be deploying the contract and posting output roots to L1. |
| `L2OO_ADDRESS` | Address of the `OPSuccinctL2OutputOracle` contract. |

## Build the Proposer Service

Build the docker images for the `op-succinct-proposer` service.

```bash
docker compose build
```

## Run the Proposer

This command launches the `op-succinct-proposer` service in the background. It launches two containers: one container that manages proof generation and another container that is a small fork of the original `op-proposer` service.

After a few minutes, you should see the `op-succinct-proposer` service start to generate range proofs. Once enough range proofs have been generated, they will be verified in an aggregate proof and submitted to the L1.

```bash
docker compose up
```

To see the logs of the `op-succinct-proposer` service, run:

```bash
docker compose logs -f
```

and to stop the `op-succinct-proposer` service, run:

```bash
docker compose stop
```