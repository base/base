# EigenDA Data Availability

This section describes the requirements to use OP Succinct for a chain with EigenDA as the data availability layer. The requirements are additive to the ones required for the `op-succinct` service. Please refer to the [Proposer](../proposer.md) section for the base configuration.

## Environment Setup

Create a `.env` file with all base configuration variables from the [Proposer](../proposer.md) section, plus the EigenDA-specific variable below.

### Required Variables

| Parameter | Description |
|-----------|-------------|
| `EIGENDA_PROXY_ADDRESS` | Base URL of the EigenDA Proxy REST service (e.g., `http://localhost:3100`). OP Succinct connects to this proxy to retrieve and validate EigenDA blobs from DA certificates. |

## EigenDA Proxy

The EigenDA Proxy is a REST server that wraps EigenDA client functionality and conforms to the OP Alt-DA server spec. It provides:

- POST routes: Disperse payloads into EigenDA and return a DA certificate.
- GET routes: Retrieve payloads via a DA certificate; performs KZG and certificate verification.

See [EigenDA Proxy](https://github.com/Layr-Labs/eigenda/tree/master/api/proxy) for more details on how to run the proxy.

After running the proxy, set `EIGENDA_PROXY_ADDRESS=http://127.0.0.1:3100` in your `.env` for OP Succinct to consume the proxy.

## EigenDA Contract Configuration

Before deploying or updating contracts, generate the EigenDA-specific verification key commitments and rollup config hash with the correct feature flag. This ensures the range verification key commitment matches the EigenDA range ELF:

```bash
# From the repository root
cargo run --bin config --release --features eigenda -- --env-file .env
```

The command prints the `Range Verification Key Hash`, `Aggregation Verification Key Hash`, and `Rollup Config Hash`; keep these values and ensure they match what you publish on-chain in `OPSuccinctL2OutputOracle`.

Whenever you rely on `just` helpers (`deploy-oracle`, `update-parameters`, etc.), include the `eigenda` argument so `fetch-l2oo-config` runs with the EigenDA feature enabled. If you invoke the Rust binaries directly, add `--features eigenda`; otherwise the script emits the default Ethereum DA values and your contracts will revert with `ProofInvalid()` when submitting proofs.

## Run the EigenDA Proposer Service

Run the `op-succinct-eigenda` service.

```bash
docker compose -f docker-compose-eigenda.yml up -d
```

To see the logs of the `op-succinct-eigenda` service, run:

```bash
docker compose -f docker-compose-eigenda.yml logs -f
```

To stop the `op-succinct-eigenda` service, run:

```bash
docker compose -f docker-compose-eigenda.yml down
```

## Deploying `OPSuccinctL2OutputOracle` with EigenDA features

```bash
just deploy-oracle .env eigenda
```

## Updating `OPSuccinctL2OutputOracle` Parameters

```bash
just update-parameters .env eigenda
```

For more details on updating parameters, see the [Updating `OPSuccinctL2OutputOracle` Parameters](../contracts/update-parameters.md) section.
