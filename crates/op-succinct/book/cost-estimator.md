# Cost Estimator

We provide a convenient CLI tool to estimate the RISC-V cycle counts (and cost) for generating ZKPs for a range of blocks for a given rollup.

## Overview

First, add the following RPCs to your `.env` file for your rollup:

```bash
# L1 RPC
L1_RPC=
# L1 Consensus RPC
L1_BEACON_RPC=
# L2 Archive Node (OP-Geth)
L2_RPC=
```

It is required that the L2 RPC is an archival node for your OP stack rollup, with the "debug_dbGet" endpoint enabled.

Then run the following command:
```shell
RUST_LOG=info just cost-estimator <start_l2_block> <end_l2_block>
```

This command will execute `op-succinct` as if it's in production. First, it will divide the entire block range
into smaller ranges optimized along the span batch boundaries. Then it will fetch the required data for generating the ZKP for each of these ranges, and execute the SP1 `span` program. Once each program finishes, it will collect the statistics and output the aggregate statistics
for the entire block range. From this data, you can extrapolate the cycle count to a cost based on the cost per billion cycles.

## Example

On Optimism Sepolia, proving the block range 15840000 to 15840050 (50 blocks) generates 4 span proofs, takes ~1.8B cycles and
~2 minutes to execute.

```bash
RUST_LOG=info just cost-estimator 15840000 15840050

...Execution Logs...

+--------------------------------+---------------------------+
| Metric                         | Value                     |
+--------------------------------+---------------------------+
| Batch Start                    |                16,240,000 |
| Batch End                      |                16,240,050 |
| Execution Duration (seconds)   |                       130 |
| Total Instruction Count        |             1,776,092,063 |
| Oracle Verify Cycles           |               237,150,812 |
| Derivation Cycles              |               493,177,851 |
| Block Execution Cycles         |               987,885,587 |
| Blob Verification Cycles       |                84,995,660 |
| Total SP1 Gas                  |             2,203,604,618 |
| Number of Blocks               |                        51 |
| Number of Transactions         |                       160 |
| Ethereum Gas Used              |                43,859,242 |
| Cycles per Block               |                74,736,691 |
| Cycles per Transaction         |                23,422,603 |
| Transactions per Block         |                        11 |
| Gas Used per Block             |                 3,509,360 |
| Gas Used per Transaction       |                 1,105,066 |
| BN Pair Cycles                 |                         0 |
| BN Add Cycles                  |                         0 |
| BN Mul Cycles                  |                         0 |
| KZG Eval Cycles                |                         0 |
| EC Recover Cycles              |                 9,407,847 |
+--------------------------------+---------------------------+
```

## Misc
- For large enough block ranges, the RISC-V SP1 program will surpass the SP1 memory limit. Recommended limit is 20-30 blocks.
- Your L2 node must have been synced for the blocks in the range you are proving. 

