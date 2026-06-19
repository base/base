# base-execution-eip8130-rpc-node

`BaseNodeExtension` wiring for the standalone EIP-8130
`eth_getTransactionCount` override defined in
[`base-execution-eip8130-rpc`].

Self-gates on a boolean: if flashblocks is enabled, this extension does
nothing (flashblocks's own override already extends
`eth_getTransactionCount` with the EIP-8130 `nonce_key` parameter, and
`jsonrpsee`'s `replace_configured` is overwrite). If flashblocks
is disabled, this extension registers `Eip8130EthApiExt` so non-zero
`nonce_key` lookups still work on standalone nodes.

The node-assembly site is the source of truth for which path is active. It passes `flashblocks_config.is_none()` as the enable flag.
