# base-execution-eip8130-rpc-node

`BaseNodeExtension` wiring for the standalone EIP-8130
`eth_getTransactionCount`, `eth_estimateGas` and `eth_call` overrides
defined in `base-execution-eip8130-rpc`.

`Eip8130RpcExtension` registers `Eip8130EthApiExt` for `nonce_key` lookups and
Everest-gated EIP-8130 gas estimation and calls. Plain estimation and call
requests delegate to the standard reth implementation.

The extension uses reth's `TransportRpcModules::replace_configured`, which
overwrites all three methods on configured transports, so no other extension
may register them.
