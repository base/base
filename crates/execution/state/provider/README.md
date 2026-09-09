# `base-execution-state-provider`

Persistent and in-memory views of Base chain state.

Providers implement historical reads, canonical state access, proof generation, block and receipt
storage, static-file access, and state updates. Overlay providers combine a persistent base with
in-memory trie changes without mutating the underlying read-only view.

The `test-utils` feature exposes provider fixtures. `partial-persistence` preserves sparse trie
persistence support; `rayon` enables parallel overlay construction.

This implementation incorporates local Reth provider and storage-overlay code.
