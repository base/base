# Celestia Data Availability

This section describes the requirements to use OP Succinct Lite for a chain with Celestia DA. The requirements are additive to the ones required for the OP Succinct Lite. Please refer to the [Proposer](../proposer.md) section for the base configuration. Also, please refer to the [Docker Setup](../docker.md) section for details on how to run the OP Succinct Lite with Docker.

## Environment Setup

Create a single `.env` file in the `fault-proof` directory with all configuration variables.

### Required Variables

Include all the base configuration variables from the [Proposer](../proposer.md) section, plus these Celestia-specific variables:

| Parameter | Description |
|-----------|-------------|
| `CELESTIA_CONNECTION` | URL of the Celestia light node RPC endpoint. For setup instructions, see [Celestia's documentation on light node](https://docs.celestia.org/operate/data-availability/light-node/quickstart). |
| `NAMESPACE` | Namespace ID for the Celestia DA. A namespace is a unique identifier that allows applications to have their own data availability space within Celestia's data availability layer. For more details, see [Celestia's documentation on Namespaced Merkle Trees (NMTs)](https://docs.celestia.org/learn/how-celestia-works/data-availability-layer#namespaced-merkle-trees-nmts). |
| `CELESTIA_INDEXER_RPC` | URL of the op-celestia-indexer RPC endpoint. This is required for querying the location of L2 blocks in Celestia or Ethereum DA. |
| `START_L1_BLOCK` | Starting L1 block number for the indexer to begin syncing from. **Note:** This should be early enough to cover derivation of the starting L2 block from the contract. |
| `BATCH_INBOX_ADDRESS` | Address of the batch inbox contract on L1. |

<div class="warning">
When using Celestia DA, ensure L1 node has sufficient proof history (archive node recommended). If the L1 node doesn't have a sufficient proof history, you may see `error distance to target block exceeds maximum proof window` when making `eth_getProof` RPC calls if proposer falls behind.
</div>

## op-celestia-indexer

The op-celestia-indexer is a required component that indexes L2 block locations on both Celestia DA and Ethereum DA. It is automatically included in the `docker-compose-celestia.yml` configuration and will start before the OP Succinct Lite services.

The indexer runs on port 57220 and uses the environment variables from your `.env` file. When using docker-compose, the `CELESTIA_INDEXER_RPC` is automatically set to `http://op-celestia-indexer:57220` for inter-container communication.

For more details about op-celestia-indexer, see the [official documentation](https://github.com/celestiaorg/optimism/blob/celestia-develop/op-celestia/readme.md).

### Running the indexer separately (optional)

If you need to run the indexer separately without docker-compose:

```bash
docker pull opcelestia/op-celestia-indexer:op-node-v1.13.6-rc.1

# Create a directory for the indexer database
mkdir -p ~/indexer-data

docker run -d \
  --name op-celestia-indexer \
  -p 57220:57220 \
  -v ~/indexer-data:/data \
  opcelestia/op-celestia-indexer:op-node-v1.13.6-rc.1 \
  op-celestia-indexer \
  --start-l1-block <START_L1_BLOCK> \
  --batch-inbox-address <BATCH_INBOX_ADDRESS> \
  --l1-eth-rpc <L1_RPC> \
  --l2-eth-rpc <L2_RPC> \
  --op-node-rpc <L2_NODE_RPC> \
  --rpc.enable-admin \
  --rpc.addr 0.0.0.0 \
  --rpc.port 57220 \
  --da.rpc <CELESTIA_CONNECTION> \
  --da.namespace <NAMESPACE> \
  --db-path /data/indexer.db
```

When running separately, set `CELESTIA_INDEXER_RPC=http://localhost:57220` in your `.env` file.

## Celestia Contract Configuration

CelestiaDA deployments also require CelestiaDA-specific verification key commitments and rollup config hashes. Always compile these values with the `celestia` feature flag so the range verification key commitment aligns with the Celestia range ELF, the aggregation verification key, and the rollup config hash:

```bash
# From the repository root
cargo run --bin config --release --features celestia -- --env-file fault-proof/.env
```

The command prints the `Range Verification Key Hash`, `Aggregation Verification Key Hash`, and `Rollup Config Hash`. Confirm these values before updating on-chain storage in `OPSuccinctFaultDisputeGame`.

When you use the `just` helper below, include the `celestia` argument so `fetch-fault-dispute-game-config` runs with the correct feature set. If you run `fetch-fault-dispute-game-config` manually, append `--features celestia`; otherwise the script emits the default Ethereum DA values and your games will revert with `ProofInvalid()` when submitting proofs.

## Deploying `OPSuccinctFaultDisputeGame` with Celestia features

```bash
just deploy-fdg-contracts .env celestia
```

## Run Services with Celestia DA

```bash
# Navigate to the fault-proof directory
cd fault-proof

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
