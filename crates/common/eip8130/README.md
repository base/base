# base-common-eip8130

Engine-neutral EIP-8130 execution primitives, shared by every Base execution engine.

This crate is the single source of truth for the EIP-8130 pieces that are pure functions of the
transaction (plus a few state-derived hints) and therefore do not depend on any particular EVM
implementation:

- [`Eip8130GasSchedule`] — the per-component intrinsic-gas cost table (Base's current schedule).
- [`IntrinsicGas`] / [`IntrinsicGasInput`] — the intrinsic-gas computation for a signed EIP-8130
  transaction.
- [`NonceManagerSlots`] — the ERC-7201 storage-slot derivation for the 2D nonce manager.

It operates purely over the engine-neutral EIP-8130 types from `base-common-consensus`, depends only
on `alloy-primitives`, and is `no_std`. The revm execution path (`base-execution-eip8130`,
`base-common-precompiles`) and the EVM2 execution path (`base-common-evm2`) both consume these
primitives so the two engines cannot diverge on intrinsic-gas pricing or nonce-slot layout.
