# op-alloy-consensus

OP Stack consensus interface.

This crate contains constants, types, and functions for implementing Optimism EL consensus and communication. This
includes transactions, [EIP-2718] envelopes, [EIP-2930], [EIP-4844], [deposit transactions][deposit], and receipts.

In general a type belongs in this crate if it exists in the `alloy-consensus` crate, but was modified from the base Ethereum protocol in the OP Stack.
For consensus types that are not modified by the OP Stack, the `alloy-consensus` types should be used instead.

[alloy-network]: ../network
[EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
[EIP-2930]: https://eips.ethereum.org/EIPS/eip-2930
[EIP-4844]: https://eips.ethereum.org/EIPS/eip-4844
[deposit]: https://specs.optimism.io/protocol/deposits.html

## Provenance

Much of this code was ported from [reth-primitives] as part of ongoing alloy migrations.

[reth-primitives]: https://github.com/paradigmxyz/reth/tree/main/crates/primitives
