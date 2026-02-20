## `base-alloy-consensus`

Base chain consensus interface.

This crate contains constants, types, and functions for implementing Base EL consensus and communication. This
includes an extended `OpTxEnvelope` type with deposit transactions, and receipts containing
chain-specific fields (`deposit_nonce` + `deposit_receipt_version`).

In general a type belongs in this crate if it exists in the `alloy-consensus` crate, but was modified from the base Ethereum protocol in the OP Stack.
For consensus types that are not modified by the OP Stack, the `alloy-consensus` types should be used instead.

### Provenance

Much of this code was ported from [reth-primitives] as part of ongoing alloy migrations, and originally from [op-alloy].

[reth-primitives]: https://github.com/paradigmxyz/reth/tree/main/crates/primitives
[op-alloy]: https://github.com/alloy-rs/op-alloy
