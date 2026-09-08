# `base`

Unified Base node binary.

## `base rpc`

`base rpc` starts a validator-oriented node by launching an embedded execution node and an embedded
consensus node in the same process. Consensus calls the execution driver and payload builder directly
and reads canonical state from the local database.

The execution CLI surface comes from `base-execution-cli`. `base rpc` intentionally filters out flags for roles it does not run, including
sequencer, builder, and conductor options.

Supported forms:

```text
base rpc
base --chain sepolia rpc
base -c sepolia rpc
base --chain zeronet rpc
base --chain dev rpc
base --chain ./chain.toml rpc
base -c ./chain.toml rpc
```

The command also accepts an execution chain override when the root `--chain` selection is used only
for consensus chain resolution:

```text
base rpc --execution-chain dev
```

The command also accepts metering flags such as `--enable-metering` for trusted local devnet
simulation nodes.

### Follow mode and historical proofs

`base rpc --source-l2-rpc <url> --http` follows another L2 node using the embedded
execution service. Add `--follow.proofs` to gate sync on local proofs history progress.
Initialize historical proofs storage before the first launch:

```bash
base reth init --chain <genesis.json> --datadir <data>
base proofs init --chain <genesis.json> --datadir <data> --proofs-history.storage-path <proofs>
base --chain <chain.toml> rpc --execution-chain <genesis.json> --datadir <data> \
  --http --http.api eth,debug --source-l2-rpc <source> --follow.proofs \
  --proofs-history --proofs-history.storage-path <proofs> \
  --l1-eth-rpc <l1-rpc> --l1-beacon <l1-beacon>
```

The Docker image runs `base rpc` by default. The root Compose file runs one integrated
node and exposes execution RPC on 8545 and consensus RPC on 7545. Configure L1 endpoints
in `.env.mainnet` or `.env.sepolia`. Add execution flags, including pruning options, to
the Compose command. Existing execution databases can be reused through `HOST_DATA_DIR`.

## `base sequencer`

`base sequencer` starts a sequencing node with embedded execution, full-block builder, and consensus
services. Consensus uses the same native execution handle as validator mode.

The command accepts the shared execution flags, builder flags, and sequencer consensus flags. It
requires L1 execution and beacon RPC endpoints, and sequencer mode requires a signing key provided
by one of `--p2p.sequencer.key`, `--p2p.sequencer.key.path`, or `--p2p.signer.endpoint`.

Supported forms:

```text
base sequencer --l1-eth-rpc <url> --l1-beacon <url> --p2p.sequencer.key.path <path>
base --chain sepolia sequencer --l1-eth-rpc <url> --l1-beacon <url> --p2p.signer.endpoint <url>
base --chain dev sequencer --l1-eth-rpc <url> --l1-beacon <url> --p2p.sequencer.key.path <path>
base --chain ./chain.toml sequencer --l1-eth-rpc <url> --l1-beacon <url> --p2p.sequencer.key.path <path>
```

Useful sequencer-specific flags include:

- `--sequencer.stopped` starts the process with sequencing disabled until the admin API starts it.
- `--sequencer.recover` enables recovery mode and forces empty block production.
- `--conductor.rpc` enables conductor-backed leader checks.
- `--conductor.binary-commit` uses the conductor binary commit endpoint.

## `base update`

`base update` updates the installed `base` binary by running `baseup --bin base` against the same
directory as the currently running executable. `baseup` downloads the `GitHub` release artifact,
checks the archive checksum, verifies the release signature, and installs the verified binary.

Supported forms:

```text
base update
base update --install v0.6.0
base update --update-installer
```

## Chain Selection

Chain selection supports:

- built-in names: `mainnet`, `sepolia`, `zeronet`, `dev`
- TOML files for custom chains:

```toml
name = "custom-chain"
l2_chain_id = 84532
l1_chain_id = 11155111
```

TOML values can be overridden with environment variables using the `BASE_CHAIN_` prefix.
