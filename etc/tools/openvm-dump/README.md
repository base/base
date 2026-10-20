# openvm-dump

Fetches one L2 block range from live RPCs and writes the `OpenVM` `--input` JSON
the range guest expects (`0x01` + rkyv `DefaultWitnessData`) as `input.json`,
plus the same witness as a bincode `StdIn` in `input.bin` for an Axiom Edge
manager's `/upload_input`.

This is the host-side dump for a zeronet (or any) block. It does not start a
system-test stack.

```bash
cargo run -p base-openvm-dump -- \
  --l1-rpc "$OPENVM_L1_RPC" \
  --l1-beacon-rpc "$OPENVM_L1_BEACON_RPC" \
  --l2-rpc "$OPENVM_L2_RPC" \
  --l2-node-rpc "$OPENVM_L2_NODE_RPC" \
  --out-dir crates/proof/zk/programs/openvm/elf
```

Omit `--start-block` / `--end-block` to dump the current safe L2 head as a
1-block range. Then execute it locally:

```bash
just openvm run
```
