# Troubleshooting

## Common Issues

### Missing Trie Node Error

**Error Message:**
```
Error: server returned an error response: error code -32000: missing trie node <hash> (path) state <hash> is not available, not found
```

**Cause:**
This error occurs when your L1 archive node has not fully synced all historical state data. It's common when:
- The archive node was recently synced
- You're attempting to prove blocks where the complete state data is not yet archived in the node

**Solution:**
1. Wait for your L1 archive node to fully sync more historical state data (typically takes about an hour, depending on your batcher interval)
2. Try proving blocks that are definitely included in your node's archived data:
   - With more recent blocks
   - Or wait longer for older blocks to be fully synced

### L2 Block Validation Failure

**Error Message:**
```
Failed to validate L2 block #<block_number> with output root <hash>
```

**Cause:**
This error occurs when the L1 head block selected for ETH DA is too close to the batch posting block, causing the derivation process to fail. The L2 node may have an inconsistent view of the safe head state, requiring additional L1 blocks to properly derive and validate the L2 blocks.

**Solution:**
1. Increase the L1 head offset buffer in the derivation process. The code currently adds a buffer of 20 blocks after the batch posting block, but you can increase this to for example 100 blocks:

```rust
let l1_head_number = l1_head_number + 100;
```

2. If you're still encountering this error, you can try:
   - Waiting for more L1 blocks to be produced and retry
   - Using a different L2 node with a more consistent safe head state
   - For development/testing, you can increase the buffer further (e.g., to 150 blocks)

**Technical Details:**
The error occurs in the derivation pipeline when attempting to validate L2 blocks. The L1 head must be sufficiently ahead of the batch posting block to ensure all required data is available and the safe head state is consistent. The buffer of 20 blocks is added empirically to handle most cases where RPCs may have an incorrect view of the safe head state and have minimum overhead for the derivation process.

Reference: [Fetcher Implementation](https://github.com/succinctlabs/op-succinct/blob/5dfc43928c75cef0ebf881d10bd8b3dcbe273419/utils/host/src/fetcher.rs#L773)
