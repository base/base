# Cycle Counts [Cost Estimator]

## Overview

To get the number of cycles to prove a given block range, first add the following RPCs to your `.env` file:

```bash
# L1 RPC
L1_RPC=
# L1 Consensus RPC
L1_BEACON_RPC=
# L2 Archive Node (OP-Geth)
L2_RPC=
```

Then run the following command:
```shell
RUST_LOG=info just run-multi <start_l2_block> <end_l2_block>
```

This will fetch the data for generating the validity proof for the given block range, "execute" the
corresponding SP1 program, and return the cycle count. Then, you can extrapolate the cycle count
to a cost based on the cost per billion cycles.

Proofs over a span batch are split into several "span proofs", which prove the validity of a section of the span batch. Then, these span proofs are aggregated into a single proof, which is submitted to the L1.

## Example Block Range

On OP Sepolia, generating a proof from 15840000 to 15840050 (50 blocks) takes ~1.5B cycles and takes
~2 minutes to execute.

```bash
RUST_LOG=info just run-multi 15840000 15840050

...Execution Logs...

+--------------------------------+---------------------------+
| Metric                         | Value                     |
+--------------------------------+---------------------------+
| Total Cycles                   |             1,502,329,547 |
| Block Execution Cycles         |             1,009,112,508 |
| Total Blocks                   |                        51 |
| Total Transactions             |                       202 |
| Cycles per Block               |                19,786,519 |
| Cycles per Transaction         |                 4,995,606 |
| Transactions per Block         |                         3 |
| Total Gas Used                 |                52,647,751 |
| Gas Used per Block             |                 1,032,308 |
| Gas Used per Transaction       |                   260,632 |
+--------------------------------+---------------------------+
```

## Misc
- For large enough block ranges, the RISC-V SP1 program will surpass the SP1 memory limit. Recommended limit is 20-30 blocks.
- Your L2 node must have been synced for the blocks in the range you are proving. 
   - OP Sepolia Node: Synced from block 15800000 onwards.
   - OP Mainnet Node: Synced from block 122940000 onwards.

