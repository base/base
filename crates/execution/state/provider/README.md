# `base-execution-state-provider`

Persistent and in-memory views of Base chain state.

Providers implement historical reads, canonical state access, proof generation, block and receipt
storage, static-file access, and state updates. Overlay providers combine a persistent base with
in-memory trie changes without mutating the underlying read-only view. Canonical in-memory
tracking, pending state, preserved sparse tries, and notification streams live here alongside
the persistent providers. Shared block and notification data remains in `base-execution-state-types`.

The `test-utils` feature exposes provider fixtures; `rayon` enables parallel overlay construction.
Partial-persistence configuration has been removed. Persisted compatibility markers retain their
existing encodings so unsupported historical layouts can still be identified.

This implementation incorporates local Reth provider, chain-state, and storage-overlay code.
