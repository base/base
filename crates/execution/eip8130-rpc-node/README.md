# base-execution-eip8130-rpc-node

`BaseNodeExtension` wiring for the standalone EIP-8130
`eth_getTransactionCount`, `eth_estimateGas` and `eth_call` overrides
defined in `base-execution-eip8130-rpc`.

With `Eip8130RpcMode::Register`, this extension registers `Eip8130EthApiExt`
for `nonce_key` lookups and Everest-gated EIP-8130 gas estimation and calls.
Plain estimation and call requests delegate to the standard reth implementation.
With `Defer`, this extension does nothing, so flashblocks owns all three methods
with its own pending-state handling.

Both extensions use reth's `TransportRpcModules::replace_configured`, which
overwrites all three methods on configured transports. The node-assembly site
must designate exactly one owner, this extension or flashblocks. It selects
the mode from the flashblocks config.
