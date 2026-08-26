# base-common-evm2

Scaffold for Base's [EVM2](https://github.com/alloy-rs/evm2) integration.

EVM2 replaces revm's trait-based composition with a single associated-type
family, [`evm2::EvmTypesHost`], plus a transaction registry and static handler
hooks. This crate anchors that family for Base as [`BaseEvmTypes`], currently
mirroring the stock Ethereum configuration.

Base-specific execution — deposit and EIP-8130 transactions (via a
`TxRegistry`), OP L1 fee settlement (via `TxHandlerHooks`), the Base spec
schedule, and L1 block info (via `BlockEnvExt`) — is layered on here in
follow-up work.

This crate is intentionally **not** wired into the node. It exists so the type
family and its (eventual) neutral precompile/state plumbing can be built and
differentially tested against the revm engine before the upstream
`alloy-evm`/`reth` EVM2 bridge lands.
