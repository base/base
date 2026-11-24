# base-tiger

The `tiger` binary and the node code behind it: `tiger node` (the Base execution client: Engine
API, JSON-RPC, EL sync over the eth protocol, and a base-reth storage v2 datadir) and
`tiger diff-adapter`, the differential fuzz adapter (see `etc/tools/diff-fuzz/PROTOCOL.md`).
`tiger node` takes base-reth's flag names; `tiger` without arguments lists them.

## Stateless start

`tiger node --stateless.upstream http://<base-reth>:8545 [--stateless.start <block|tag>]` on an
empty datadir starts at the upstream's block (default `finalized`) with none of its state, as a
partial state that holds only what blocks have needed. The start block is checked against its
header (transactions and receipts roots) and the upstream's chain id. A block whose execution
reads state the datadir does not hold is validated over the upstream's execution witness
(`debug_executionWitnessByBlockHash`, else `debug_executionWitness`), each node verified by hash
against the parent's state root and each ancestor header by hash, and the witness is stored with
the block when it becomes canonical. History begins at the start block; blocks of a branch that
forks below it cannot be followed. The upstream needs the `debug` and `eth` RPC modules and must
hold the state of the blocks tiger catches up on.

RPC reads of state the datadir does not hold are answered from the upstream's `eth_getProof` at
the same block, verified against tiger's state root, or fail with "state not local". The upstream
needs an `--rpc.eth-proof-window` covering how far behind its head tiger's queried blocks are
(reth's default 0 answers only at its head).

## Edge coverage

With `BASE_DIFF_COVERAGE=1` the adapter adds a `coverage` map of `tiger:edge:<index>` features to
build-mode responses. The counters exist only in an instrumented build:

```sh
cd tiger
SANCOV_CRATES="base_tiger_evm base_tiger evm2" \
RUSTC_WRAPPER=$PWD/../etc/tools/diff-adapter/sancov-rustc.sh CARGO_TARGET_DIR=target/sancov \
  cargo build --release -p base-tiger --features edge-coverage
```
