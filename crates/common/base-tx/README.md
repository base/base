# base-common-base-tx

Engine-neutral Base transaction execution primitives, shared by every Base execution engine.

This crate is the single source of truth for the Base transaction pieces that are pure functions of the
transaction and therefore do not depend on any particular EVM implementation:

- [`TxBaseGasSchedule`] — the per-component intrinsic-gas cost table (Base's current schedule).
- [`IntrinsicGas`] — the intrinsic-gas computation for a signed Base transaction and its
  resolved sender, with no state reads.
- [`NonceManagerSlots`] — the ERC-7201 storage-slot derivation for the 2D nonce manager.

It operates purely over the engine-neutral Base transaction types from `base-common-consensus`, with
`alloy-primitives` for primitive types. It supports `no_std` when default
features are disabled. The revm execution path (`base-execution-base-tx`, `base-common-precompiles`)
and the EVM2 execution path (`base-common-evm2`) both consume these primitives so the two engines
cannot diverge on intrinsic-gas pricing or nonce-slot layout.
