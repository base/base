# Celestia Data Availability

This section describes the requirements to use OP Succinct for a chain with Celestia DA. The requirements are additive to the ones required for the `op-succinct` service. Please refer to the [Proposer](../proposer.md) section for the base configuration.

## Environment Setup

To use Celestia DA, you need additional environment variables in the `.env` file:

| Parameter | Description |
|-----------|-------------|
| `CELESTIA_CONNECTION` | URL of the Celestia light node RPC endpoint. For setup instructions, see [Celestia's documentation on light node](https://docs.celestia.org/how-to-guides/light-node). |
| `NAMESPACE` | Namespace ID for the Celestia DA. A namespace is a unique identifier that allows applications to have their own data availability space within Celestia's data availability layer. For more details, see [Celestia's documentation on Namespaced Merkle Trees (NMTs)](https://docs.celestia.org/learn/how-celestia-works/data-availability-layer#namespaced-merkle-trees-nmts). |

## Run the Celestia Proposer Service

Run the `op-succinct-celestia` service.

```bash
docker compose -f docker-compose-celestia.yml up -d
```

To see the logs of the `op-succinct-celestia` service, run:

```bash
docker compose -f docker-compose-celestia.yml logs -f
```

To stop the `op-succinct-celestia` service, run:

```bash
docker compose -f docker-compose-celestia.yml down
```

## Deploying `OPSuccinctL2OutputOracle` with Celestia features

```bash
just deploy-oracle .env celestia
```

## Updating `OPSuccinctL2OutputOracle` Parameters

```bash
just update-parameters .env celestia
```

For more details on the `just update-parameters` command, see the [Updating `OPSuccinctL2OutputOracle` Parameters](../contracts/update-parameters.md) section.
