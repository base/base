# op-rbuilder

[![CI status](https://github.com/flashbots/rbuilder/actions/workflows/checks.yaml/badge.svg?branch=develop)](https://github.com/flashbots/rbuilder/actions/workflows/integration.yaml)


`op-rbuilder` is a Rust-based block builder designed to build blocks for the Optimism stack. 

## Running op-rbuilder

To run op-rbuilder with the op-stack, you need:
- CL node to sync the op-rbuilder with the canonical chain
- Sequencer with the [rollup-boost](https://github.com/flashbots/rollup-boost) setup

To run the op-rbuilder, run:

```bash
cargo run -p op-rbuilder --bin op-rbuilder --features flashblocks -- node \
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

1. Clone [flashbots/builder-playground](https://github.com/flashbots/builder-playground) and start an OPStack chain.

```bash
git clone https://github.com/flashbots/builder-playground.git
cd builder-playground
go run main.go cook opstack --external-builder http://host.docker.internal:4444
```

2. Remove any existing `reth` chain db. The following are the default data directories:

- Linux: `$XDG_DATA_HOME/reth/` or `$HOME/.local/share/reth/`
- Windows: `{FOLDERID_RoamingAppData}/reth/`
- macOS: `$HOME/Library/Application Support/reth/`

3. Run `op-rbuilder` in the `rbuilder` repo on port 4444:

```bash
cargo run -p op-rbuilder --bin op-rbuilder -- node \
    --chain $HOME/.playground/devnet/l2-genesis.json \
    --http --http.port 2222 \
    --authrpc.port 4444 --authrpc.jwtsecret $HOME/.playground/devnet/jwtsecret \
    --port 30333 --disable-discovery \
    --metrics 127.0.0.1:9001 \
    --rollup.builder-secret-key ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
    --trusted-peers enode://3479db4d9217fb5d7a8ed4d61ac36e120b05d36c2eefb795dc42ff2e971f251a2315f5649ea1833271e020b9adc98d5db9973c7ed92d6b2f1f2223088c3d852f@127.0.0.1:30304
```

4. Init `contender`:

```bash
git clone https://github.com/flashbots/contender
cd contender
cargo run -- setup ./scenarios/simple.toml http://localhost:2222
```

6. Run `contender`:

```bash
cargo run -- spam ./scenarios/simple.toml http://localhost:2222 --tpb 10 --duration 10
```

And you should start to see blocks being built and landed on-chain with `contender` transactions.
