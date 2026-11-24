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
