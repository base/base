# Proposer

The `op-succinct` service monitors the state of the L2 chain, requests proofs from the [Succinct Prover Network](https://docs.succinct.xyz/docs/sp1/prover-network/intro) and submits them to the L1.

## RPC Requirements

Confirm that your RPC's have all of the required endpoints as specified in the [prerequisites](../advanced/node-setup.md#required-accessible-endpoints) section.

## Hardware Requirements

We recommend the following hardware configuration for the default `op-succinct` validity service (1 concurrent proof request & 1 concurrent witness generation thread):

Using the docker compose file:

- Full `op-succinct` service: 2 vCPUs, 4GB RAM.
- Mock `op-succinct` service: 2 vCPUs, 8GB RAM. Increased memory because the machine is executing the proofs locally.

Depending on the number of concurrent requests you want to run, you may need to increase the number of vCPUs and memory allocated to the `op-succinct` container.

## Environment Setup

Make sure to include *all* of the required environment variables in the `.env` file.

Before starting the proposer, ensure you have deployed the relevant contracts and have the address of the proxy contract ready. Follow the steps in the [Environment Variables](./contracts/environment.md) section.

### Required Environment Variables

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L1_BEACON_RPC` | L1 Consensus (Beacon) Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |
| `NETWORK_PRIVATE_KEY` | Private key for the Succinct Prover Network. See the [Succinct Prover Network Quickstart](https://docs.succinct.xyz/docs/sp1/prover-network/quickstart) for setup instructions. |
| `L2OO_ADDRESS` | Address of the `OPSuccinctL2OutputOracle` contract. |
| `PRIVATE_KEY` | Private key for the account that will be posting output roots to L1. |

### Optional Environment Variables

| Parameter | Description |
|-----------|-------------|
| `NETWORK_RPC_URL` | Default: `https://rpc.production.succinct.xyz`. RPC URL for the Succinct Prover Network. |
| `DATABASE_URL` | Default: `postgres://op-succinct@postgres:5432/op-succinct`. The address of a Postgres database for storing the intermediate proposer state. |
| `DGF_ADDRESS` | Address of the `DisputeGameFactory` contract. Note: If set, the proposer will create a dispute game with the DisputeGameFactory, rather than the `OPSuccinctL2OutputOracle`. Compatible with `OptimismPortal2`. |
| `RANGE_PROOF_STRATEGY` | Default: `reserved`. Set to `hosted` to use hosted proof strategy. |
| `AGG_PROOF_STRATEGY` | Default: `reserved`. Set to `hosted` to use hosted proof strategy. |
| `AGG_PROOF_MODE` | Default: `groth16`. Set to `plonk` to use PLONK proof type. Note: The verifier gateway contract address must be updated to use PLONK proofs. |
| `SUBMISSION_INTERVAL` | Default: `1800`. The number of L2 blocks that must be proven before a proof is submitted to the L1. Note: The interval used by the validity service is always >= to the `submissionInterval` configured on the L2OO contract. To allow for the validity service to configure this parameter entirely, set the `submissionInterval` in the contract to `1`. |
| `RANGE_PROOF_INTERVAL` | Default: `1800`. The number of blocks to include in each range proof. For chains with high throughput, you need to decrease this value. |
| `MAX_CONCURRENT_PROOF_REQUESTS` | Default: `1`. The maximum number of concurrent proof requests (in mock and real mode). |
| `MAX_CONCURRENT_WITNESS_GEN` | Default: `1`. The maximum number of concurrent witness generation requests. |
| `OP_SUCCINCT_MOCK` | Default: `false`. Set to `true` to run in mock proof mode. The `OPSuccinctL2OutputOracle` contract must be configured to use an `SP1MockVerifier`. |
| `METRICS_PORT` | Default: `8080`. The port to run the metrics server on. |
| `LOOP_INTERVAL` | Default: `60`. The interval (in seconds) between each iteration of the OP Succinct service. |
| `SIGNER_URL` | URL for the Web3Signer. Note: This takes precedence over the `PRIVATE_KEY` environment variable. |
| `SIGNER_ADDRESS` | Address of the account that will be posting output roots to L1. Note: Only set this if the signer is a Web3Signer. Note: Required if `SIGNER_URL` is set. |
| `SAFE_DB_FALLBACK` | Default: `false`. Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not activated for op-node.  When `false`, proposer will panic if SafeDB is not available. It is by default `false` since using the fallback mechanism will result in higher proving cost. |
| `OP_SUCCINCT_CONFIG_NAME` | Default: `"opsuccinct_genesis"`. The name of the configuration the proposer will interact with on chain. |
| `OTLP_ENABLED` | Default: `false`. Whether to export logs to [OTLP](https://opentelemetry.io/docs/specs/otel/protocol/). |
| `LOGGER_NAME` | Default: `op-succinct`. This will be the `service.name` exported in the OTLP logs. |
| `OTLP_ENDPOINT` | Default: `http://localhost:4317`. The endpoint to forward OTLP logs to. |

## Build the Proposer Service

Build the OP Succinct validity service.

```bash
docker compose build
```

## Run the Proposer

Run the OP Succinct validity service.

```bash
docker compose up
```

To see the logs of the OP Succinct services, run:

```bash
docker compose logs -f
```

After several minutes, the validity service will start to generate range proofs. Once enough range proofs have been generated, an aggregation proof will be created. Once the aggregation proof is complete, it will be submitted to the L1.

To stop the OP Succinct validity service, run:

```bash
docker compose stop
```
