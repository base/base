# EigenDA Data Availability

This section describes the requirements to use OP Succinct Lite for a chain with EigenDA as the data availability layer. The requirements are additive to the ones required for OP Succinct Lite. Please refer to the [Proposer](../proposer.md) section for the base configuration, and the [Running with Docker](../docker.md) section for general Docker guidance.

## Environment Setup

Create two env files in the `fault-proof` directory with all required configuration variables:

- `.env.proposer` for the proposer service
- `.env.challenger` for the challenger service

Include all base variables from the [Proposer](../proposer.md) section, plus the EigenDA-specific variable below. Use the same variable across both env files so both components can access EigenDA data if needed.

### Required Variables

| Parameter | Description |
|-----------|-------------|
| `EIGENDA_PROXY_ADDRESS` | Base URL of the EigenDA Proxy REST service (e.g., `http://localhost:3100`). OP Succinct Lite connects to this proxy to retrieve and validate EigenDA blobs from DA certificates. |

## EigenDA Proxy

The EigenDA Proxy is a REST server that wraps EigenDA client functionality and conforms to the OP Alt-DA server spec. It provides:

- POST routes: Disperse payloads into EigenDA and return a DA certificate.
- GET routes: Retrieve payloads via a DA certificate; performs KZG and certificate verification.

See [EigenDA Proxy](https://github.com/Layr-Labs/eigenda/tree/master/api/proxy) for more details on how to run the proxy.

After running the proxy, set `EIGENDA_PROXY_ADDRESS=http://127.0.0.1:3100` in the `.env.proposer` file.

## EigenDA Contract Configuration

EigenDA deployments also require EigenDA-specific verification key commitments and rollup config hashes. Always compile these values with the `eigenda` feature flag so the range verification key commitment aligns with the EigenDA range ELF, the shared aggregation verification key is re-derived, and the rollup config hash mirrors the configuration returned by your rollup node:

```bash
# From the repository root
cargo run --bin config --release --features eigenda -- --env-file fault-proof/.env
```

The command prints the `Range Verification Key Hash`, `Aggregation Verification Key Hash`, and `Rollup Config Hash`. Confirm these values before updating on-chain storage in `OPSuccinctFaultDisputeGame`.

When you use the `just` helper below, include the `eigenda` argument so `fetch-fault-dispute-game-config` runs with the correct feature set. If you run `fetch-fault-dispute-game-config` manually, append `--features eigenda`; otherwise the script emits the default Ethereum DA values and your games will revert with `ProofInvalid()` when submitting proofs.

## Deploying `OPSuccinctFaultDisputeGame` with EigenDA features

```bash
just deploy-fdg-contracts .env eigenda
```

## Run Services with EigenDA DA

```bash
# Navigate to the fault-proof directory
cd fault-proof

# Start both proposer and challenger (EigenDA)
docker compose -f docker-compose-eigenda.yml up -d
```

To see the logs, run:

```bash
docker compose -f docker-compose-eigenda.yml logs -f
```

To stop the services, run:

```bash
docker compose -f docker-compose-eigenda.yml down
```
