# Local Devnets

This repository runs several local environments. Pick one by what you need to test, then follow its
owning doc for details. App developers can start from the
[Run a Local Devnet](https://docs.base.org/build-on-base/run-a-local-devnet) guide on docs.base.org.

| Environment | Use it for | Start | State and L1 |
|---|---|---|---|
| Docker devnet | Full-stack sequencing, derivation, batching, hardfork activation | `just devnet up-single` | Fresh genesis in `.devnet/`; Reth + Lighthouse L1 |
| Docker HA devnet | Conductor leadership and failover | `just devnet up` | Same as above with three conductor-managed sequencers |
| Anvil + local Nitro proving | Proof generation and proposer integration without Nitro hardware | `just devnet build-anvil-image`, then `just anvil-nitro-local up` | Fresh `.devnet/`; Base-Anvil L1; contracts bypass attestation |
| Snapshot devnet (`base dev`) | Real Base Mainnet or Sepolia contracts and state, impersonating any account, 200ms Denim blocks | `just devnet snapshot download --dir D`, then `just devnet snapshot run --dir D` | One disposable snapshot datadir; no L1 |
| Snapshot benchmark harness | Builder/follow-client experiments and snapshot performance work | `cargo run -p base-system-tests --bin base-devnet -- snapshot --builder-datadir B --client-datadir C` | Two writable copies of one snapshot; no L1 |

Snapshot environments have no L1 derivation, bridges, safe/finalized heads, or proofs. Use a Docker
devnet to test those.

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
