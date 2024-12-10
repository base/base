# Proposer

The `op-succinct` service consists of two containers:
- `op-succinct-server`: Receives proof requests from the `op-succinct-proposer`, generates the witness for the proof, and submits the proof to the Succinct Prover Network. Handles the communication with the [Succinct's Prover Network](https://docs.succinct.xyz/generating-proofs/prover-network) to fetch the proof status and completed proof data.
- `op-succinct-proposer`: Monitors L1 state to determine when to request a proof. Sends proof requests to the `op-succinct-server`. Once proofs have been generated for a sufficiently large range, aggregates range proofs into an aggregation proof. Submits the aggregation proof to the `OPSuccinctL2OutputOracle` contract which includes the L2 state outputs.

We've packaged the `op-succinct` service in a docker compose file to make it easier to run.

## Prerequisites

### RPC Requirements

Confirm that your RPC's have all of the required endpoints. More details can be found in the [prerequisites](../quick-start/prerequisites.md#requirements) section.

### Hardware Requirements

We recommend the following hardware configuration for the `op-succinct` service containers:

Using the docker compose file:

- `op-succinct`: 16 vCPUs, 16GB RAM

Running as separate containers:

- `op-succinct-server`: 16 vCPUs, 16GB RAM
- `op-succinct-proposer`: 1 vCPU, 4GB RAM

For advanced configurations, depending on the number of concurrent requests you expect, you may need to increase the number of vCPUs and memory allocated to the `op-succinct-server` container.

## Environment Setup

### Required Environment Variables

Before starting the proposer, the following environment variables should be in your `.env` file. You should have already set up your environment when you deployed the L2 Output Oracle. If you have not done so, follow the steps in the [Contract Configuration](../contracts/configuration.md) section.

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

### Advanced Environment Variables

The following environment variables are optional.

| Parameter | Description |
|-----------|-------------|
| `MAX_CONCURRENT_PROOF_REQUESTS` | Default: `10`. The maximum number of concurrent proof requests to send to the `op-succinct-server`. |
| `MAX_BLOCK_RANGE_PER_SPAN_PROOF` | Default: `300`. The maximum number of blocks to include in each span proof. For chains with high throughput, you need to decrease this value. |
| `OP_SUCCINCT_MOCK` | Default: `false`. Set to `true` to run in mock proof mode. The `OPSuccinctL2OutputOracle` contract must be configured to use an `SP1MockVerifier`. |
| `OP_SUCCINCT_SERVER_URL` | Default: `http://op-succinct-server:3000`. The URL of the `op-succinct-server` service which the `op-succinct-proposer` will send proof requests to. |
| `METRICS_ENABLED` | Default: `true`. Set to `false` to disable metrics collection. |
| `METRICS_PORT` | Default: `7300`. The port to run the metrics server on. |
| `DB_PATH` | Default: `/usr/local/bin/dbdata`. The path to the database directory within the container. |
| `POLL_INTERVAL` | Default: `20s`. The interval at which the `op-succinct-proposer` service runs. |
| `USE_CACHED_DB` | Default: `false`. Set to `true` to use cached proofs from previous runs when restarting the service, avoiding regeneration of unused proofs. |

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