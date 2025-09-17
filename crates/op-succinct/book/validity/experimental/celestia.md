# Celestia Data Availability

This section describes the requirements to use OP Succinct for a chain with Celestia DA. The requirements are additive to the ones required for the `op-succinct` service. Please refer to the [Proposer](../proposer.md) section for the base configuration.

## Environment Setup

To use Celestia DA, you need additional environment variables.

**Important:** When using Docker Compose, these variables must be in a `.env` file in the same directory as your `docker-compose-celestia.yml` file. Docker Compose needs these variables in `.env` for variable substitution in the compose file itself.

| Parameter | Description |
|-----------|-------------|
| `CELESTIA_CONNECTION` | URL of the Celestia light node RPC endpoint. For setup instructions, see [Celestia's documentation on light node](https://docs.celestia.org/how-to-guides/light-node). |
| `NAMESPACE` | Namespace ID for the Celestia DA. A namespace is a unique identifier that allows applications to have their own data availability space within Celestia's data availability layer. For more details, see [Celestia's documentation on Namespaced Merkle Trees (NMTs)](https://docs.celestia.org/learn/how-celestia-works/data-availability-layer#namespaced-merkle-trees-nmts). |
| `CELESTIA_INDEXER_RPC` | URL of the op-celestia-indexer RPC endpoint. This is required for querying the location of L2 blocks in Celestia or Ethereum DA. |
| `START_L1_BLOCK` | Starting L1 block number for the indexer to begin syncing from. **Note:** This should be early enough to cover derivation of the starting L2 block from the contract. |
| `BATCH_INBOX_ADDRESS` | Address of the batch inbox contract on L1. |

Additionally, include all the base configuration variables from the [Proposer](../proposer.md) section in the same `.env` file.

<div class="warning">
When using Celestia DA, ensure L1 node has sufficient proof history (archive node recommended). If the L1 node doesn't have a sufficient proof history, you may see `error distance to target block exceeds maximum proof window` when making `eth_getProof` RPC calls if proposer falls behind.
</div>

## op-celestia-indexer

The op-celestia-indexer is a required component that indexes L2 block locations on both Celestia DA and Ethereum DA. It is automatically included in the `docker-compose-celestia.yml` configuration and will start before the OP Succinct proposer service.

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
  --da.auth_token <CELESTIA_AUTH_TOKEN> \
  --db-path /data/indexer.db
```

When running separately, set `CELESTIA_INDEXER_RPC=http://localhost:57220` in your `.env` file.

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

## Celestia Contract Configuration

Before deploying or updating contracts, generate the CelestiaDA-specific range verification key, aggregation verification key, and rollup config hash with the correct feature flag. This ensures the range verification key commitment matches the Celestia range ELF:

```bash
# From the repository root
cargo run --bin config --release --features celestia -- --env-file .env
```

The command prints the `Range Verification Key Hash`, `Aggregation Verification Key Hash`, and `Rollup Config Hash`; keep these values and ensure they match what you publish on-chain in `OPSuccinctL2OutputOracle`.

When you use the `just` helpers below, pass the `celestia` feature so `fetch-l2oo-config` runs with the correct ELFs. If you call the binaries manually (`fetch-l2oo-config`, `config`, etc.), append `--features celestia`; otherwise the script emits the default Ethereum DA values and your contracts will revert with `ProofInvalid()` when submitting proofs.

## Deploying `OPSuccinctL2OutputOracle` with Celestia features

```bash
just deploy-oracle .env celestia
```

## Updating `OPSuccinctL2OutputOracle` Parameters

```bash
just update-parameters .env celestia
```

For more details on the `just update-parameters` command, see the [Updating `OPSuccinctL2OutputOracle` Parameters](../contracts/update-parameters.md) section.
