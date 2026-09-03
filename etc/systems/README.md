# `base-system-tests`

System-test and development-network infrastructure for Base nodes. In addition to the fresh
L1/L2 stack used by system tests, this crate can continue a Base mainnet execution snapshot with
real builder and client execution and consensus components in one managed launcher process.

## Fresh devnet sequencer

The `fresh` mode runs the same in-process builder and sequencer stack used by system tests against
an already-running development L1. The Docker devnet supplies the generated L1/L2 genesis files,
rollup configuration, L1 RPC endpoints, persistent builder datadir, and stable network ports:

```bash
cargo run -p base-system-tests --bin base-devnet --no-default-features -- fresh \
  --l1-rpc-url http://l1-el:4545 \
  --l1-beacon-url http://l1-cl:4052 \
  --p2p-key "$BUILDER_P2P_KEY" \
  --sequencer-key "$SEQUENCER_KEY" \
  --p2p-addr 172.30.0.20
```

`just devnet up-single` runs this mode in the `base-builder` container. The batcher, client, RPC
node, L1, and setup services remain separate Compose services. The HA/conductor topology continues
to use the integrated `base sequencer` command because it requires multiple independently managed
sequencers.

## Snapshot devnet topology

The snapshot mode starts these real local network roles inside one managed launcher process:

```text
snapshot builder EL <-> standalone L1-free sequencer CL
        |
        | unsafe blocks
        v
 follow-mode CL      <-> snapshot client EL
```

Both ELs start from separate writable copies of the same Base mainnet Reth datadir. The builder
mines real descendants of the captured mainnet head and the follow client canonicalizes those
blocks. Transactions submitted to the builder use its real transaction pool and normal
`eth_sendRawTransaction` path.

Interactive `base-devnet` runs the sequencer and validator concurrently.

This is an unsafe-chain development network, not a valid restartable continuation of Base mainnet.
It has no L1, derivation, batching, or safe/finalized-head advancement. At 200ms it produces full
canonical blocks with Base/Reth's standard payload service. The 2s case uses the Flashblocks payload
service, but the 200ms case neither starts nor subscribes to Flashblocks. Treat 200ms results as
full-block results and do not compare Flashblock latency against the 2s case.

## Prerequisites

Run commands from the `base/base` repository root. You need:

- the normal Rust and native build dependencies for this repository;
- an immutable Base mainnet Reth snapshot;
- two fresh, writable clones of that snapshot for each run;
- enough free space for both clones to diverge during the run; and
- Foundry's `cast` only when manually interacting with `base-devnet`.

Each supplied datadir must already exist and contain `db/mdbx.dat`. Builder and client paths must
be distinct. The launcher never creates, copies, deletes, or takes ownership of these datadirs.

For an interactive `base-devnet` session, generate a throwaway key whose address can be prefunded:

```bash
cast wallet new
export FUNDER_KEY=0x... # private key printed above; use only for this local network
cast wallet address --private-key "$FUNDER_KEY"
```

Never use a key that controls real funds.

## Prepare local ZFS clones

Keep the source dataset immutable and clone the same snapshot once for each EL. Substitute your
actual source snapshot and disposable dataset names:

```bash
export SNAPSHOT=zroot/data/snapshots/base-mainnet-06-09@latest
export BUILDER_DATASET=zroot/data/snapshots/base-mainnet-bench-builder
export CLIENT_DATASET=zroot/data/snapshots/base-mainnet-bench-client

sudo zfs clone "$SNAPSHOT" "$BUILDER_DATASET"
sudo zfs clone "$SNAPSHOT" "$CLIENT_DATASET"

export BUILDER_DATADIR=/home/user/snapshots/base-mainnet-bench-builder
export CLIENT_DATADIR=/home/user/snapshots/base-mainnet-bench-client
test -f "$BUILDER_DATADIR/db/mdbx.dat"
test -f "$CLIENT_DATADIR/db/mdbx.dat"
```

An AWS run has the same ownership contract: materialize two writable datadirs from the same
immutable snapshot on instance-local `NVMe`, then pass their paths. Snapshot and volume lifecycle is
intentionally outside `base-devnet` so the command cannot destroy caller-owned data. The simple
workflow keeps one sequencer datadir and one validator datadir for the life of the
instance and reuses them rather than rolling them back between runs; record the mutated starting
heads when doing so. Use fresh equivalent copies when strict boundary equivalence matters.

## Run a snapshot-backed development network

First derive the address for the throwaway funder key. Then start a 2s network and mint funds to
that address in the first local descendant:

```bash
export FUNDER_ADDRESS=$(cast wallet address --private-key "$FUNDER_KEY")

cargo run -p base-system-tests --bin base-devnet -- snapshot \
  --builder-datadir "$BUILDER_DATADIR" \
  --client-datadir "$CLIENT_DATADIR" \
  --block-interval 2s \
  --prefund-address "$FUNDER_ADDRESS" \
  --runtime-file /tmp/base-snapshot-runtime.json
```

Use `--block-interval 200ms` for the subsecond variant. The first descendant activates `BaseTime`
metadata and subsequent blocks advance on a deterministic 200ms schedule.

Startup validates chain ID 8453, the boundary L1-info transaction, `SystemConfig`, and sequence
number. It waits for the builder to extend the snapshot and for the client to follow before writing
the runtime file. The process then runs until Ctrl-C and shuts both EL runtimes down gracefully.

In another terminal, inspect the machine-readable endpoints and compare the live heads:

```bash
jq . /tmp/base-snapshot-runtime.json

BUILDER_RPC=$(jq -r .builder_rpc_url /tmp/base-snapshot-runtime.json)
CLIENT_RPC=$(jq -r .client_rpc_url /tmp/base-snapshot-runtime.json)

cast chain-id --rpc-url "$BUILDER_RPC"
cast block-number --rpc-url "$BUILDER_RPC"
cast block-number --rpc-url "$CLIENT_RPC"
cast balance "$FUNDER_ADDRESS" --rpc-url "$BUILDER_RPC"
```

The runtime JSON contains `status`, `chain_id`, `boundary_number`, `boundary_hash`,
`block_interval_ms`, `builder_rpc_url`, `builder_flashblocks_url`, and `client_rpc_url`. Dynamic
ports are the default and are safest for automation. `--stable-ports` binds the builder and client
RPCs to ports 7545 and 8545, respectively, but fails if those ports are occupied.

To pin a run to a known snapshot boundary, pass all three of `--expected-head-number`,
`--expected-head-hash`, and `--expected-head-timestamp`. Startup fails before load generation if
the captured boundary differs.

## Cleanup

After Ctrl-C, verify that no process still has either mount open, then destroy only the disposable
datasets:

```bash
sudo zfs destroy "$BUILDER_DATASET"
sudo zfs destroy "$CLIENT_DATASET"
```

Never destroy the immutable source snapshot. If shutdown was interrupted, check for a lingering
`base-devnet` process before unmounting or destroying storage.

## Troubleshooting

- **Missing `db/mdbx.dat`:** pass the Reth datadir root, not its `db` directory or a ZFS mount that
  contains another nesting level.
- **Same builder/client path:** create two clones. One database cannot safely serve both roles.
- **Boundary mismatch:** recreate both clones from the intended immutable snapshot or correct all
  three expected-head flags; do not partially relax boundary validation.
- **Address delegation or funding errors:** prefund a newly generated throwaway address via
  `--prefund-address` instead of the standard Anvil development address, whose delegated-account
  state at the tested snapshot can trip Reth's delegated-account in-flight limit while funding
  senders.
- **Port conflict:** omit `--stable-ports` and consume the allocated URLs from runtime JSON.
- **Unexpected disk growth:** account creation changes state heavily. Monitor ZFS referenced space
  or EBS free space throughout long runs.
- **No 200ms Flashblock data:** expected. The 200ms snapshot stack uses Base/Reth's standard payload
  service and does not publish or subscribe to Flashblocks; the 2s path remains unchanged. Compare
  canonical blocks, confirmations, gas, and throughput instead.

See the exact supported options at any revision with:

```bash
cargo run -p base-system-tests --bin base-devnet -- snapshot --help
```
