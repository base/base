# Local Devnets

This repository runs several local environments. Pick one by what you need to test, then follow its
owning doc for details. App developers can start from the
[Run a Local Devnet](https://docs.base.org/build-on-base/run-a-local-devnet) guide on docs.base.org.

| Environment | Use it for | Start | State and L1 |
|---|---|---|---|
| Anvil with Base precompiles | Contract development and tests against Base precompiles; starts in seconds | `anvil --hardfork base:Cobalt` (Foundry v1.8.4+) or `docker run --rm -p 8545:8545 ghcr.io/base/base-anvil:main --host 0.0.0.0 --base denim` | Empty dev chain or a `--fork-url` fork; no sequencer, L1, or Flashblocks |
| Docker devnet | Full-stack sequencing, derivation, batching, hardfork activation | `just devnet up-single` | Fresh genesis in `.devnet/`; Reth + Lighthouse L1 |
| Docker HA devnet | Conductor leadership and failover | `just devnet up` | Same as above with three conductor-managed sequencers |
| Anvil + local Nitro proving | Proof generation and proposer integration without Nitro hardware | `just devnet build-anvil-image`, then `just anvil-nitro-local up` | Fresh `.devnet/`; Base-Anvil L1; contracts bypass attestation |
| Snapshot devnet (`base dev`) | Real Base Mainnet or Sepolia contracts and state, impersonating any account, 200ms Denim blocks | `just devnet snapshot download --dir D`, then `just devnet snapshot run --dir D` | One disposable snapshot datadir; no L1 |
| Snapshot benchmark harness | Builder/follow-client experiments and snapshot performance work | `cargo run -p base-system-tests --bin base-devnet -- snapshot --builder-datadir B --client-datadir C` | Two writable copies of one snapshot; no L1 |

Snapshot environments have no L1 derivation, bridges, safe/finalized heads, or proofs. Use a Docker
devnet to test those.

## Anvil With Base Precompiles

For contract developers who need Base's native precompiles (B20 factory and tokens, PolicyRegistry,
ActivationRegistry) without running a node. Either option is ready in about three seconds.

For new projects, use Foundry v1.8.4 or later, as [Foundry for Base](https://docs.base.org/sdks/base-anvil)
on docs.base.org recommends:

```sh
anvil --hardfork base:Cobalt                                      # empty chain, chain ID 31337
anvil --fork-url https://sepolia.base.org --hardfork base:Cobalt  # Base detected from the chain ID
```

As of v1.8.6, Foundry's built-in Base schedule ends at Beryl, so pass `--hardfork base:Cobalt` when
forking too. Plain `anvil` runs Ethereum, where the precompile addresses have no code. On an empty
chain the ActivationRegistry admin is the zero address unless you pass
`--base-activation-admin <address>`.

To test the precompiles on this repository's `main` before a Foundry release includes them, use
`ghcr.io/base/base-anvil`. [base-anvil-package.yml](../../.depot/workflows/base-anvil-package.yml)
builds `anvil` and `forge` from [base/base-anvil](https://github.com/base/base-anvil) against this
repository's `crates/common/precompiles` and `crates/common/chains`, smoke-tests them with base-std
fork tests and `--base denim`, and publishes `main` and immutable `sha-<commit>` tags on every push
to `main`.

```sh
docker run --rm -p 8545:8545 ghcr.io/base/base-anvil:main --host 0.0.0.0 --base denim
cast chain-id                                                            # 31337
cast call 0x8453000000000000000000000000000000000001 'admin()(address)'  # ActivationRegistry admin
```

`--base` takes `azul`, `beryl`, `cobalt`, or `denim`; a bare `--base` uses the build's default.
Without `--base` the image is stock Anvil and precompile addresses have no code. The ActivationRegistry
admin defaults to prefunded account 5 (`0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc`), including on a
`--fork-url` fork; pass `--base-activation-admin <address>` to use the forked chain's admin. The image's
entrypoint is `anvil`; run `forge` with `--entrypoint forge`, and read the source commits with
`--entrypoint cat ghcr.io/base/base-anvil:main /usr/local/share/base-anvil/MANIFEST.json`.

Both options run the precompiles inside Foundry's EVM, not Base's node, so they do not model:

- A Base sequencer or builder: Anvil mines its own blocks, and there is no L1 or derivation.
- L1 data fees, BaseTime millisecond timestamps, EIP-8130 transactions, and deposit transactions,
  which may behave differently from Base or be missing.
- Flashblocks.

Use the snapshot devnet or the Docker devnet when you need those.

## Docker Devnet

Requires Docker with Compose v2 and Buildx, [`just`](https://github.com/casey/just) `1.51.0` or newer,
`jq`, and Foundry `cast`.

```sh
just devnet up-single   # or `just devnet up` for the HA cluster
just devnet status      # block numbers and sync status
just devnet checks      # status plus L1/L2 smoke transactions
just devnet down
```

`up`, `up-single`, and `down` delete everything in `.devnet/`. The sequencer RPC is
`http://127.0.0.1:7545` and the RPC node is `http://127.0.0.1:8545`; prefunded test keys and ports are in
`etc/docker/devnet-env`. Pass `everest` or `zenith` to `up`/`up-single` to schedule those forks. Run
`just devnet` for every command. Topology, batchers, and images: [etc/docker/README.md](../../etc/docker/README.md).

## Anvil + Local Nitro Proving

```sh
just devnet build-anvil-image
just anvil-nitro-local up
just anvil-nitro-local status
just anvil-nitro-local down
```

`up` and `down` start from and remove fresh data. Do not use Anvil timestamp-warp RPCs here. Details:
[etc/docker/README.md](../../etc/docker/README.md#single-anvil-l1-local-nitro-proving).

## Snapshot Devnet

Builds `base` natively with Cargo; Docker is not required. In October 2026 a mainnet download was
about 830 GiB and about 1.4 TB on disk; add `--print-plan-json` to `download` to see current sizes.

```sh
just devnet snapshot download --dir "$HOME/data/my-devnet"
just devnet snapshot run --dir "$HOME/data/my-devnet"
```

`run` continues the snapshot as a local chain with RPC on `http://127.0.0.1:8545` and
`ws://127.0.0.1:8546`. The first run rebuilds indices before producing blocks. Stop with Ctrl+C and
rerun the same command to resume; the datadir keeps its development blocks, so restore a clean copy
to start over. Fund accounts and act as any address with `dev_impersonateTransaction`.
Prerequisites, retention, timing, and RPC reference: [bin/base/README.md](../../bin/base/README.md#base-dev).

## Snapshot Benchmark Harness

Runs a builder and a follow client on two writable copies of one snapshot, with benchmark-specific
block gas limits. Every attempt needs a fresh datadir pair. Running it:
[etc/systems/README.md](../../etc/systems/README.md); benchmarks:
[SNAPSHOT_BENCHMARKS.md](SNAPSHOT_BENCHMARKS.md).

## Tests

- [TESTING.md](TESTING.md): system tests against the Docker devnet (`just devnet tests`).
- [acceptance/README.md](../../acceptance/README.md): TOML scenarios that provision their own Docker
  devnet; stop any running devnet first.
- [STANDALONE_PROVING.md](STANDALONE_PROVING.md): the prover stack, which the Docker devnet does not
  include.
