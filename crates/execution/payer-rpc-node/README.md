# base-execution-payer-rpc-node

`BaseNodeExtension` wiring that registers the ERC-8168 `payer_*` RPC
([`base-execution-payer-rpc`]) on a Base node.

It supplies the two node-specific collaborators the handler needs:

- [`StateBackedPayerTerms`] — resolves the per-block
  [`PriceSnapshot`](base_execution_payer::PriceSnapshot) by
  decoding the on-chain payer config against the node's latest committed state.
  It wraps the state provider in a read-only precompile-storage adapter
  ([`base_precompile_storage::ReadOnlyStorage`]) and runs
  `PayerConfigStorage::price_snapshot`, so terms are a handful of `SLOAD`s with
  no EVM execution.
- the payer co-signer key (a local secp256k1 key today).

The extension is gated behind [`PayerRpcConfig::enabled`] and only registers
when a payer key is present, mirroring the metering extension's enable pattern.

[`base-execution-payer-rpc`]: ../payer-rpc/README.md
