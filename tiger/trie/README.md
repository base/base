# `base-tiger-trie`

Merkle Patricia trie code and the state database of the tiger client.

- `StateDb` (`src/state_db.rs`) keeps state in a base-reth storage v2 datadir: flat hashed
  state, trie nodes, changesets and history. It commits and unwinds blocks, and its views read
  accounts, storage, code, roots and proofs at any stored block with in-memory blocks on top.
- Full state uses reth's trie tables (`AccountsTrie` / `StoragesTrie`) and a port of reth's
  trie walker (`src/walker.rs`, `src/root.rs`).
- Partial state (`src/partial.rs`) is for a node that starts at a recent block without state.
  It fills from each block's execution witness and keeps trie nodes as RLP by path. Reads of
  state not held locally fail as unknown (`TrieError::is_unknown`); they never answer zero or
  absent for it.
- `SparseTrie` (`src/sparse.rs`) loads nodes lazily from a witness, the database, or proofs. The
  partial state's roots and proofs use it, and so does `StateView::trie_witness`, which lists the
  nodes a block's root computation needs.
- `TrieView::state_root_parallel` walks the account trie as 16 subtrees, each first computing
  the changed storage roots of its accounts (storages with over 32 changed slots on jobs of their
  own, so two under one subtree are not hashed one after the other), on the calling thread and the idle threads of `WorkerPool` (`src/pool.rs`), threads
  started once and kept: a root waits only for jobs a worker claimed, never for a thread to be
  scheduled, so a busy machine slows it to at worst one thread's time.

## Verification of the state root

Stored branch rows follow reth: `hash_mask` bit `i` when child `i` is a branch, `tree_mask` bit `i`
when the child's topmost branch is stored; a branch is stored when either is non-empty, and the
root never is.

Tested by randomized differential checks:
- `TrieView` over a datadir: 12 seeds × 30 blocks of account, slot, wipe and deletion changes
  against a `HashBuilder` model (roots, proofs, and every few blocks the stored rows against a
  from-scratch rebuild), with the parallel root equal to the sequential one, also over an
  in-memory layer while the datadir advances.
- The node iterator skips the hashed seek after a skipped subtree, and on entering a changed
  subtree whose branch is stored, when the walker reached its next node through stored masks and
  no changed key lies between: checked by running every skipped seek anyway and asserting its
  result would be dropped, over all tests, `root_bench`, sync seeds 13 and 23 and 300 s state
  campaigns (no case found); dropping either condition fails the model test.

Contract and assumptions:
- No branch node encodes to under 32 bytes (that needs two keys sharing 62 or more nibbles of
  their keccak); `HashBuilder` panics on it too.
- keccak (alloy-primitives), RLP headers (alloy-rlp) and alloy-trie's node encodings are trusted.
