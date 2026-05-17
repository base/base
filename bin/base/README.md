# `base`

Unified Base node binary.

## `base rpc`

`base rpc` starts a validator-oriented node by launching an embedded execution node and an embedded
consensus node in the same process. The execution node exposes the Engine API over auth IPC, and the
consensus node connects to that IPC endpoint internally.

The execution CLI surface is shared with the standalone execution binaries through
`base-execution-cli`. `base rpc` intentionally filters out flags for roles it does not run, including
sequencer, builder, conductor, metering, and transaction-forwarding options.

Supported forms:

```text
base rpc
base --chain sepolia rpc
base -c sepolia rpc
base --chain zeronet rpc
base --chain ./chain.toml rpc
base -c ./chain.toml rpc
```

The command also accepts an execution chain override when the root `--chain` selection is used only
for consensus chain resolution:

```text
base rpc --execution-chain dev
```

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

- built-in names: `mainnet`, `sepolia`, `zeronet`
- TOML files for custom chains:

```toml
name = "custom-chain"
l2_chain_id = 84532
l1_chain_id = 11155111
```

TOML values can be overridden with environment variables using the `BASE_CHAIN_` prefix.
