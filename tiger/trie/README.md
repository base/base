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
- `LiveTrie` / `LiveState` (`src/live.rs`) keep the trie nodes blocks touched in memory across
  blocks, revealed lazily from the stored branch rows and flat leaves, in 16 shards per trie. A
  block's root hashes only what changed, and its trie updates are the stored rows that change.
  `StateDb` keeps one for the canonical head; it applies only when the view's state root equals
  the root it was left at, else it starts over from the stored trie. It is off unless
  `TIGER_LIVE_ROOT=1` (it gives no gain on real blocks yet and its memory is not bounded well);
  `TIGER_LIVE_ROOT=check` also walks every root and panics if the two differ. Above
  `TIGER_LIVE_MAX_NODES` nodes (1,000,000, about 120 bytes each plus leaf values), the least
  recently used hashed branches are blinded, down to half of it. `LiveSession`
  (`src/stream.rs`) feeds it each transaction's changes while the block executes, on a
  background thread, and reconciles with the block's final changes.

## Verification of the state root

`Spec` (`src/spec.rs`) is the reference: a from-scratch recursive build of the trie (Yellow Paper
appendix D) that gives the root and reth's stored branch rows (`hash_mask`: child is a branch;
`tree_mask`: the child's topmost branch is stored; a branch is stored when either is non-empty;
the root never is). Everything else is checked against it or against the walker.

Proved (Kani, `cargo kani` in `trie/`, harnesses under `cfg(kani)`):
- hex-prefix encoding is injective and round-trips for every nibble string up to 6 long.

Tested exhaustively:
- `Spec` equals alloy-trie's `HashBuilder` (root and stored rows) on every subset of five
  families of up to 16 keys that differ in chosen nibble positions (deep shared prefixes,
  extensions, embedded leaves), plus 300 random tries up to 3,000 keys.

Tested by randomized differential checks:
- `LiveTrie`: 60 seeds × 40 blocks of inserts, updates and deletes against `Spec` (root, and the
  stored rows after applying each block's updates), dropping the trie or pruning it at random
  so reveals from the stored rows are exercised; structural invariants (`LiveTrie::check`)
  after every block.
- `LiveState`: 40 seeds × 30 blocks of account, storage, wipe and deletion changes against
  `Spec` for every trie, on 1 to 4 threads, pruned to half its size every third block.
- Over a datadir: the live state against the walker (`TrieView::state_root`) in the incremental
  model test, root equal and trie updates with the same effect.
- End to end (`tests/live.rs`): blocks of transactions streamed into a `LiveSession` (values
  changed and changed back, wipes, deletions and re-creations, blocks with nothing streamed;
  `LiveSession::flush` makes the background apply each transaction, some of them, or none
  before the final changes arrive; half the seeds cap the trie at 1,500 nodes so it prunes
  every block), committed into one datadir with the session's trie updates and into another
  with the walker's: roots equal and the two trie tables byte-identical after every block.
- Mutation checks: removing the deletion of collapsed rows, the tree-mask or hash-mask rules,
  the stream's reconciliation steps, or a blinded branch's storedness each fail these tests.

Contract and assumptions:
- Trie updates are the walker's minus rewrites of rows that do not change (the walker rewrites
  every stored node it walks through), so the tables after applying them are identical.
- No branch node encodes to under 32 bytes (that needs two keys sharing 62 or more nibbles of
  their keccak); `HashBuilder` panics on it too.
- keccak (alloy-primitives), RLP headers (alloy-rlp) and alloy-trie's node encodings are trusted.
  A Kani proof that an account leaf's storage root sits at a fixed offset from its end (which
  `LiveState` overwrites in place) runs CBMC out of memory (over 28 GB) on alloy-rlp's `U256`
  encoding even with nonce and balance below 256; it is covered by the `LiveState` tests.
- Streamed values are never trusted: the final changes decide the result.
