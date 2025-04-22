# Celestia Data Availability

This section describes the requirements to use OP Succinct Lite for a chain with Celestia DA. The requirements are additive to the ones required for the OP Succinct Lite. Please refer to the [Proposer](../proposer.md) section for the base configuration. Also, please refer to the [Docker Setup](../docker.md) section for details on how to run the OP Succinct Lite with Docker.

## Environment Setup

To use Celestia DA, you need additional environment variables in the `.env` file:

| Parameter | Description |
|-----------|-------------|
| `CELESTIA_CONNECTION` | URL of the Celestia light node RPC endpoint. For setup instructions, see [Celestia's documentation on light node](https://docs.celestia.org/how-to-guides/light-node). |
| `NAMESPACE` | Namespace ID for the Celestia DA. A namespace is a unique identifier that allows applications to have their own data availability space within Celestia's data availability layer. For more details, see [Celestia's documentation on Namespaced Merkle Trees (NMTs)](https://docs.celestia.org/learn/how-celestia-works/data-availability-layer#namespaced-merkle-trees-nmts). |

## Run Services with Celestia DA

```bash
# Navigate to the fault_proof directory
cd fault_proof

# Start both proposer and challenger
docker compose -f docker-compose-celestia.yml up -d
```

To see the logs, run:

```bash
docker compose -f docker-compose-celestia.yml logs -f
```

To stop the services, run:

```bash
docker compose -f docker-compose-celestia.yml down
```
