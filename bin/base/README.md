# `base`

Unified Base node binary.

## OTLP Tracing

Unified commands, including `rpc`, `follow`, and `sequencer`, support the same
OTLP span-export options as the standalone nodes. Set
`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_PROTOCOL` (`http` or
`grpc`), and `OTEL_SERVICE_NAME`, or use their CLI equivalents:

```text
base rpc --tracing-otlp=http://localhost:4317 --tracing-otlp-protocol=grpc --tracing-otlp.filter=info,reth=debug
```

Tracing flags work before or after the subcommand. `--tracing-otlp.filter`
controls exported spans independently of stdout/file logging verbosity, and
`--tracing-otlp.sample-ratio` controls sampling. Export is disabled unless an
endpoint is configured; HTTP endpoints use `/v1/traces` and gRPC endpoints use
the collector address without that path.

## `base batcher`

`base batcher` submits L2 batch data to L1. It uses the shared logging and metrics
flags and `BASE_NODE_LOG_*` / `BASE_NODE_METRICS_*` variables (metrics port: 9090).
L1 RPC uses `--l1-rpc-url` / `BASE_NODE_L1_ETH_RPC`;
`--l1-eth-rpc` remains an alias. Other batcher settings use `BASE_BATCHER_*`,
including `BASE_BATCHER_PRIVATE_KEY` and `BASE_BATCHER_SEQUENCER_URLS`.

The batcher discovers chain configuration from its RPC endpoints, including the
batch inbox and fork configuration from the rollup config of the node whose
derivation it follows. Top-level `--chain` / `BASE_CHAIN` selection is rejected.

```text
base batcher --l1-rpc-url <url> --sequencer-urls <url>[,<url>...] --private-key <key>
```

The former standalone binary's `BATCHER_*` environment variables are replaced by
these shared or `BASE_BATCHER_*` names.

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

## `base dev`

From a checkout, download a snapshot once (optional if you already have a disposable copy), then
run it in the foreground:

```sh
just devnet snapshot download --dir "$HOME/data/my-devnet"
just devnet snapshot run --dir "$HOME/data/my-devnet"
```

These native recipes incrementally build `base` with Cargo; Docker is not required. Install the
repository's [Rust toolchain](../../rust-toolchain.toml), `just`, Go, a C/C++ compiler, libclang,
`OpenSSL` development headers, pkg-config, `CMake`, and protoc. The RPC examples below additionally use
Foundry `cast` and `jq`. See [CONTRIBUTING.md](../../CONTRIBUTING.md) for development tooling;
bare `just devnet snapshot` lists the commands.

**Use disposable data, never a running node's datadir or your only mainnet copy.** Development blocks
and minted balances permanently diverge from mainnet. To return to mainnet, restore a clean backup;
deleting `base-dev.json` does not undo execution. Only one process may open the database for writing.

`base dev` continues an existing snapshot datadir as a local chain: an embedded execution node and
an L1-free standalone sequencer in one process, connected over Engine IPC. It never creates,
downloads, or reinitializes data. Discovery and peers are disabled, and HTTP and WS RPC listen on
loopback. A pre-Denim head gets Denim at its next block; the schedule and snapshot head are saved to
`base-dev.json` in the datadir before the first block and restored on restart. Block timestamps
continue from the snapshot.

HTTP defaults to `http://127.0.0.1:8545`, WS to `ws://127.0.0.1:8546`; both expose ordinary `eth_*`,
`debug_*`, and `trace_*` methods. Keep these unauthenticated endpoints private: impersonation can act
as any account. Startup prints endpoints and the starting head; database repair and history-index
rebuilds can take time before sequencing starts.

```sh
just devnet snapshot run --dir "$HOME/data/my-devnet" --http.port 9545 --ws.port 9546
# Direct binary usage; mainnet is the default.
target/release/base dev --dir "$HOME/data/my-devnet"
# For another supported chain, select the same chain for download and run.
env -u BASE_CHAIN just devnet snapshot download --chain sepolia --dir "$HOME/data/sepolia-devnet"
BASE_CHAIN=sepolia just devnet snapshot run --dir "$HOME/data/sepolia-devnet"
```

Download has its own `--chain` flag and rejects the root `BASE_CHAIN` selector; keep it unset when
downloading. Run accepts `BASE_CHAIN`, or root `--chain` before `dev` in direct binary usage.

Stop with Ctrl+C, then rerun the same command to resume. Head, state, storage layout and pruning
settings stay in the same datadir. Downtime does not produce catch-up blocks or advance logical time.
Denim targets 200ms slots, but disk/CPU load can delay arrivals and transaction inclusion. Header
timestamps have whole-second resolution; `BaseTime` deposits carry the 200ms progression. A ten-block
wait becomes roughly two logical seconds instead of twenty; a twenty-second timestamp lock remains
twenty header seconds, with up to one second of rounding relative to `BaseTime`. Neither is a
guarantee about wall-clock latency. A receipt confirms canonical execution, not a disk flush:
forced termination can lose recent in-memory blocks as well as pending requests.

This mode tests local execution, not L1-backed derivation, bridges, safe/finalized advancement or
proofs. Use the existing [Docker/Anvil devnets](../../etc/docker/README.md) for full-stack tests and
the [snapshot benchmark harness](../../etc/systems/README.md) for benchmarks.

### Download selection and interruptions

`download` uses `--minimal`: full current state and headers with only recent transactions, receipts,
and account/storage history, rounded outward to archive chunk boundaries. It stops after downloading;
it does not create a validator copy, start nodes or configure a fork. Transaction and history indices
rebuild on node startup. Fresh downloads generate `reth.toml` with the minimal preset's pruning
settings; an existing `reth.toml` is not overwritten.

Pin a manifest when resuming an interrupted download. Verified, unchanged archive outputs are
reused; an interrupted large archive may restart from the beginning. Do not rerun download over a
datadir that has already produced development blocks; resume it with `run` instead.

```sh
# Inspect the exact archives and sizes without creating a datadir or downloading archives.
just devnet snapshot download --dir "$HOME/data/my-devnet" --print-plan-json
# Add --manifest-url <published-manifest-url> to pin a particular snapshot for download/resume.
# Equivalent direct binary download:
target/release/base snapshot download --minimal --dir "$HOME/data/my-devnet"
```

### `dev_impersonateTransaction`

Only `base dev` serves this method on its HTTP and WS RPC. It executes a call as any account
without its key, so it can fund accounts and make privileged calls:

```text
dev_impersonateTransaction({from, to?, input?, value?, gas, mint?}) -> txHash
```

`from` and `gas` are required. `input` defaults to empty, and `value` and `mint` to zero. `mint` wei
is credited to `from` before the call. Omitting `to` creates a contract. The request becomes a
deposit transaction, not a signed one: it skips signature, nonce, and fee checks, and pays no fees.
Use signed transactions to test admission and fees.

A returned hash means the request is queued, not mined. Requests are added in order after the
system deposits of the next block with enough gas, alongside ordinary pool transactions. Poll
`eth_getTransactionReceipt` for the hash. A call that reverts or runs out of gas is still included,
with status `0`; minted ETH is kept even then. Each request is a distinct transaction, so repeating
it runs it again.

Requests that fail validation return error `-32602`: gas below the intrinsic or calldata floor cost,
gas above the block gas limit minus 2,000,000 for system deposits, calldata over 131,072 bytes, or
init code over 49,152 bytes. Error `-32000` means retry later: the chain has not started sequencing,
or 256 requests are already pending. Pending requests are held in memory and are lost if the process stops before
they are included.

Fund a newly generated throwaway key, wait for inclusion, then submit a normal signed transfer:

```sh
RPC=http://127.0.0.1:8545
DEV_KEY=$(cast wallet new --json | jq -r '.[0].private_key')
ACCOUNT=$(cast wallet address --private-key "$DEV_KEY")
HASH=$(cast rpc --rpc-url "$RPC" dev_impersonateTransaction \
  "{\"from\":\"$ACCOUNT\",\"to\":\"$ACCOUNT\",\"gas\":\"0x5208\",\"mint\":\"0xde0b6b3a7640000\"}" | jq -r .)
cast receipt --rpc-url "$RPC" "$HASH"
cast send --rpc-url "$RPC" --private-key "$DEV_KEY" \
  0x000000000000000000000000000000000000dEaD --value 1wei
```

For a privileged contract call, set `from` to its authorized account, `to` to the contract, and
`input` to ABI calldata (for example, `cast calldata 'setValue(uint256)' 42`). Set an adequate `gas`
limit, submit through the same method, and inspect the receipt's `status`. Application-level
signature checks inside contracts are not bypassed. Do not resubmit just because a receipt is not
available yet: a second request executes again.

### Optional container usage

An image containing this version of `base` runs the same command. On Linux, host networking keeps
the default RPC bindings on host loopback. Stop the native node before opening the same datadir:

```sh
docker run --rm -it --network host --user "$(id -u):$(id -g)" \
  -v "$HOME/data/my-devnet:/data" --entrypoint /app/base \
  "ghcr.io/base/node:<tag-containing-dev>" dev --dir /data
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

- built-in names: `mainnet`, `sepolia`, `zeronet`, `dev`
- TOML files for custom chains:

```toml
name = "custom-chain"
l2_chain_id = 84532
l1_chain_id = 11155111
```

TOML values can be overridden with environment variables using the `BASE_CHAIN_` prefix.
