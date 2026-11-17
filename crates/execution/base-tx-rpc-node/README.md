# base-execution-base-tx-rpc-node

`BaseNodeExtension` wiring for the Base transaction `eth_getTransactionCount`,
`eth_estimateGas` and `eth_call` overrides defined in
`base-execution-base-tx-rpc`.

The extension registers `TxBaseEthApiExt` for `nonce_key` lookups and
Everest-gated Base transaction gas estimation and calls. Plain estimation and call
requests delegate to the standard reth implementation.

The overrides use reth's `TransportRpcModules::replace_configured`, which
overwrites all three methods on configured transports, so no other extension
may register the same methods.
