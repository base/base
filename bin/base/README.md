# `base`

Unified Base node binary.

## `base rpc`

`base rpc` starts a validator-oriented node by launching an embedded execution node and an embedded
consensus node in the same process. The execution node exposes the Engine API over auth IPC, and the
consensus node connects to that IPC endpoint internally.

The execution CLI surface is shared with the standalone execution binaries through
`base-execution-cli`. `base rpc` intentionally filters out flags for roles it does not run, including
sequencer, builder, conductor, and transaction-forwarding options.

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

## `base sequencer`

`base sequencer` starts a sequencing node by launching an embedded execution node, embedded
Flashblocks builder, and embedded consensus node in the same process. The execution node exposes the
Engine API over auth IPC, and the consensus node connects to that IPC endpoint internally.

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
- `--flashblocks.port` selects the Flashblocks websocket port.

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
