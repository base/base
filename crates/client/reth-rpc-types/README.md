# `base-reth-rpc-types`

Re-exports reth RPC types for external consumers.

This crate provides a stable interface for external projects to depend on reth
types without directly depending on the reth crates. The following types are
re-exported:

- From `reth-rpc-eth-types`: `EthApiError`, `SignError`
- From `reth-optimism-evm`: `extract_l1_info_from_tx`
