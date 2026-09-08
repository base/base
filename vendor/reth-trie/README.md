# `reth-trie`

Merkle Patricia trie algorithms, proof generation and verification, and database-backed
account/storage cursors. The `test-utils` feature exposes fixtures and test cursors.

Parallel proof scheduling and Base proof-history storage live in `base-execution-trie`, above
the provider layer. Shared node encodings and prefix sets remain in `reth-trie-common`.
