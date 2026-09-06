# base-execution-eip8130-rpc-node

Node extension registering the EIP-8130 `eth_getTransactionCount` and `eth_estimateGas` overrides from
`base-execution-eip8130-rpc`. All execution nodes install this extension so
nonzero `nonce_key` lookups use the channel nonce at the requested block.
