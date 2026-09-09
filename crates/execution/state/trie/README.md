# `base-execution-state-trie`

Merkle Patricia trie algorithms, proof generation and verification, and database-backed
account/storage cursors. The `test-utils` feature exposes fixtures and test cursors.

Parallel proof scheduling and Base proof-history storage live in `base-execution-state-tasks`, above
the provider layer. Shared node encodings and prefix sets remain in `base-execution-state-types`.
