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
