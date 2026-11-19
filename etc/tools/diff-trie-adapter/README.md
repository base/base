# `base-diff-trie-adapter`

The base/base adapter of the differential trie fuzz harness (state root fuzzing). It answers
`trie` requests ([`TRIE_PROTOCOL.md`](../diff-fuzz/TRIE_PROTOCOL.md)): it applies each block of
hashed-key writes to every state root code path a base/base node can take, each over its own
on-disk database, and reports the state root, storage roots, and proofs after every block.

## Implementations

The primary result (`blocks`) is the node's serial state root over a reth MDBX database with the
v1 trie table layout: the root and trie updates the payload builder and the engine's synchronous
fallback compute (`StateRootProvider::state_root_with_updates` on the latest state), with the
block's hashed state and trie updates then written and committed as persistence does. Storage
roots come from the stored storage tries, and proofs from one `eth_getProof`-style multiproof.
Wiping or deleting an account zeroes every slot it has in the database, as the node's hashed post
state does (the pinned reth has no storage wipe flag).

Variants, each also on its own database:

- `engine`: the engine's default state root task, rebuilt from public parts: an
  `ArenaParallelSparseTrie` sparse state trie revealed from v2 multiproofs that the engine's proof
  workers (`reth_trie_parallel`'s `ProofWorkerHandle`) compute over the database, updated one
  transaction-sized chunk at a time (a new chunk starts at every rewrite of a key or slot in
  the block), kept across blocks with its storage roots (and pruned), with its trie updates
  persisted to a database with the v2 (packed) trie table layout. It reports storage roots and
  proofs from that database too. `engine-serial` is the serial fallback over the same database,
  and `engine-tables` reports stored trie nodes that differ from a rebuild (`reth_trie::verify`).
- `sparse-fresh` (a new sparse trie each block) and `sparse-eager` (kept across blocks, with
  parallelism thresholds of one so tiny tries take the parallel code paths), over the serial
  database before each commit.
- `overlay-sparse`: a fresh engine sparse trie over the overlay database, revealed from the
  calculator the overlay state provider builds (in-memory blocks' trie nodes and hashed state
  over the database's). `sparse-reverts`, on blocks after an unwind: the same over the serial
  database at the old tip with the unwind's reverts as the overlay and their keys as prefix
  sets, as after a reorg below the persisted tip.
- `storage-api`, `ph-rocksdb-storage-api` (preimage requests): each slot proven with
  `storage_proof`, checked against `storage_multiproof` and `storage_root`. `ph-mdbx-witness`:
  the stateless root from the proofs-history store's execution witness at the parent.
- `ph-init-mdbx`, `ph-init-rocksdb`: proofs-history stores backfilled by the node's
  initialization job (`InitializationJob`, as `base-proofs init` runs it) from the serial (v1
  trie layout, resuming a backfill stopped halfway through the hashed accounts) and engine (v2)
  databases after the request's last unwind, then fed live.
- `serial-root`: the serial root without trie updates (`StateRootProvider::state_root`). The
  overlay variant also checks `overlay_root_from_nodes` against its root with updates, and
  `ph-batch` checks each block's witness root and, with preimages, the session's storage proof
  API against its multiproof, reporting disagreements as the block's error.
- `full-rebuild`: the root from the hashed tables alone, ignoring stored nodes; `serial-tables`:
  stored trie nodes of the serial database that differ from a rebuild.
- `merkle-rebuild`: the pipeline merkle stage's rebuild mode in a transaction that is rolled
  back: the trie tables cleared, then the root computed in steps of at most 1 to 7 trie updates,
  each step's updates written and the next resumed from its checkpoint after a round trip
  through the stage's compact checkpoint encoding.
- `getproof`, `ph-mdbx-getproof` (requests whose proof targets all carry preimages): proofs by
  address and slot key as `eth_getProof` computes them, over the serial database and over the
  proofs-history store at the block.
- `witness`: the stateless root the differential execution adapter computes
  (`base_diff_adapter::WitnessState`) from the node's execution witness of the block, in the
  legacy witness mode base/base serves. (The canonical mode leaves out nodes that only a consumer
  applying removals before insertions needs, so it does not pair with this consumer.)
- `overlay`: the overlay state provider's root with the last two blocks in memory above the
  database (`overlay_root_from_nodes_with_updates`), unwinding in-memory blocks natively.
- `ph-mdbx`, `ph-rocksdb`: Base's proofs-history store (`base-execution-trie`) on both backends,
  fed as its `ExEx` does (root and updates over the store at the parent, stored per block, and
  after a reorg the dropped blocks unwound with `unwind_history` first), with roots and proofs read
  back at the block. `ph-*-pruned` also run the node's pruner, keeping as few blocks below the
  highest tip as the request's unwinds allow: the `MDBX` store after every block, one or two blocks
  per transaction, and the `RocksDB` store every third block, one block per transaction. `ph-*-history` re-reads every canonical, unpruned block's root and proofs after the
  last block, and `ph-batch` builds all blocks in one MDBX batch session (requests without
  unwinds).

Other unwinds are applied as the request's `revert` ops followed by its ops.

## Coverage

Build with the `edge-coverage` feature through `etc/tools/diff-adapter/sancov-rustc.sh` with
`SANCOV_CRATES` naming the trie crates, in its own target directory:

```sh
SANCOV_CRATES="reth_trie reth_trie_common reth_trie_db reth_trie_sparse alloy_trie \
  base_execution_trie base_proof_mpt reth_provider" \
RUSTC_WRAPPER=etc/tools/diff-adapter/sancov-rustc.sh CARGO_TARGET_DIR=target/sancov-trie \
  cargo build --release -p base-diff-trie-adapter --features edge-coverage
```

With `BASE_DIFF_COVERAGE=1` each response carries `edge:<index>` for every counter hit.
