# `base-proof-witness`

Builds the preimage vector consumed by Base fault-proof backends.

The generator fetches hash-pinned L1 derivation data and L2 execution witnesses concurrently,
validates and deduplicates their content-addressed preimages, and returns the existing
`Vec<(PreimageKey, Vec<u8>)>` wire format.

## Local benchmark

The ignored benchmark compares this generator with the current Nitro host replay on a Base
mainnet range of 600 L2 blocks with 30-block intermediate roots. It ends the range at the current
safe L2 block and derives the matching proof request from the configured RPCs.

```bash
L1_ETH_URL=<archive-l1-rpc> \
L2_ETH_URL=<archive-l2-rpc> \
L2_NODE_URL=<rollup-rpc> \
L1_BEACON_URL=<beacon-rpc> \
cargo test -p base-proof-witness benchmark_mainnet_witness_generation -- --ignored --nocapture
```

Set `WITNESS_BENCH_BLOCK_RANGE=1` to smoke-test one L2 block. This does not exercise a 30-block
intermediate-root checkpoint; the default remains the 600-block mainnet range.

The L2 RPC must expose `debug_executePayload`; the L1 RPC must expose
`debug_getRawReceipts`.
