# op-rbuilder

[![CI status](https://github.com/flashbots/rbuilder/actions/workflows/checks.yaml/badge.svg?branch=develop)](https://github.com/flashbots/rbuilder/actions/workflows/integration.yaml)


`op-rbuilder` is a Rust-based block builder designed to build blocks for the Optimism stack. 

## Running op-rbuilder

To run op-rbuilder with the op-stack, you need:
- CL node to sync the op-rbuilder with the canonical chain
- Sequencer with the [rollup-boost](https://github.com/flashbots/rollup-boost) setup

To run the op-rbuilder, run:

```bash
cargo run -p op-rbuilder --bin op-rbuilder --features optimism -- node \
    --chain /path/to/chain-config.json \
    --http \
    --authrpc.port 9551 \
    --authrpc.jwtsecret /path/to/jwt.hex 
```

To build the op-rbuilder, run:

```bash
cargo build -p op-rbuilder --bin op-rbuilder --features optimism
```

## Observability

To verify whether a builder block has landed on-chain, you can add the `--rollup.builder-secret-key` flag or `BUILDER_SECRET_KEY` environment variable. 
This will add an additional transaction to the end of the block from the builder key. The transaction will have `Block Number: {}` in the input data as a transfer to the zero address. Ensure that the key has sufficient balance to pay for the transaction at the end of the block.

To enable metrics, set the `--metrics` flag like in [reth](https://reth.rs/run/observability.html) which will expose reth metrics in addition to op-rbuilder metrics. op-rbuilder exposes on-chain metrics via [reth execution extensions](https://reth.rs/developers/exex/exex.html) such as the number of blocks landed and builder balance. Note that the accuracy of the on-chain metrics will be dependent on the sync status of the builder node. There are also additional block building metrics such as:

- Block building latency
- State root calculation latency
- Transaction fetch latency
- Transaction simulation latency
- Number of transactions included in the built block

To see the full list of op-rbuilder metrics, see [`src/metrics.rs`](./src/metrics.rs).

## Integration Testing

op-rbuilder has an integration test framework that runs the builder against mock engine api payloads and ensures that the builder produces valid blocks.

To run the integration tests, run:

```bash
# Generate a genesis file
cargo run -p op-rbuilder --bin tester --features optimism -- genesis --output genesis.json

# Build the op-rbuilder binary
cargo build -p op-rbuilder --bin op-rbuilder --features optimism

# Run the integration tests
cargo run -p op-rbuilder --bin tester --features optimism -- run
```

## Local Devnet

To run a local devnet, you can use the optimism docker compose tool and send test transactions with `mev-flood`.

1. Clone [flashbots/optimism](https://github.com/flashbots/optimism) and checkout the `op-rbuilder` branch.

```bash
git clone https://github.com/flashbots/optimism.git
cd optimism
git checkout op-rbuilder
```

2. Remove any existing `reth` chain db. The following are the default data directories:

- Linux: `$XDG_DATA_HOME/reth/` or `$HOME/.local/share/reth/`
- Windows: `{FOLDERID_RoamingAppData}/reth/`
- macOS: `$HOME/Library/Application Support/reth/`

3. Run a clean OP stack in the `optimism` repo:

```bash
make devnet-clean && make devnet-down && make devnet-up
```

4. Run `op-rbuilder` in the `rbuilder` repo on port 8547:

```bash
cargo run -p op-rbuilder --bin op-rbuilder --features optimism -- node \
    --chain ../optimism/.devnet/genesis-l2.json \
    --http \
    --http.port 8547 \
    --authrpc.jwtsecret ../optimism/ops-bedrock/test-jwt-secret.txt \
    --metrics 127.0.0.1:9001 \
    --rollup.builder-secret-key ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
```

5. Init `mev-flood`:

```bash
docker run mevflood init -r http://host.docker.internal:8547 -s local.json
```

6. Run `mev-flood`:

```bash
docker run --init -v ${PWD}:/app/cli/deployments mevflood spam -p 3 -t 5 -r http://host.docker.internal:8547 -l local.json
```

And you should start to see blocks being built and landed on-chain with `mev-flood` transactions.
