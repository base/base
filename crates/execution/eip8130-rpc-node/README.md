# base-execution-eip8130-rpc-node

`BaseNodeExtension` wiring for the EIP-8130 `eth_getTransactionCount`,
`eth_estimateGas` and `eth_call` overrides defined in
`base-execution-eip8130-rpc`.

The extension registers `Eip8130EthApiExt` for `nonce_key` lookups and
Everest-gated EIP-8130 gas estimation and calls. Plain estimation and call
requests delegate to the standard reth implementation.

The overrides use reth's `TransportRpcModules::replace_configured`, which
overwrites all three methods on configured transports, so no other extension
may register the same methods.
