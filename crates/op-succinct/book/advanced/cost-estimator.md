# Cost Estimator

We provide a convenient CLI tool to fetch the RISC-V instruction counts for generating ZKPs for a range of blocks for a given rollup. We recommend running the cost estimator on a remote machine with fast network connectivity because witness generation fetches a significant amount of data.

## Overview

In the root directory, add the following RPCs to your `.env` file for your rollup:

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L1_BEACON_RPC` | L1 Consensus (Beacon) Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |

More details on the RPC requirements can be found in the [prerequisites](../getting-started/prerequisites.md) section.

Then run the following command:
```shell
just cost-estimator <start_l2_block> <end_l2_block>
```

This command will split the block range into smaller ranges as if the `op-succinct-proposer` service was running. It will then fetch the required data for generating the ZKP for each of these ranges, and execute the SP1 `range` program. Once each program finishes, it will collect the statistics and output the aggregate statistics.

> Running the cost estimator for a large block range may be slow on machines with limited network bandwidth to the L2 node. For optimal performance, we recommend using a remote machine with high-speed connectivity to avoid slow witness generation.

## Example

On Optimism Sepolia, proving the block range 17664000 to 17664125 (125 blocks) takes 4 range proofs and ~11.1B cycles.

```bash
RUST_LOG=info just cost-estimator 17664000 17664125

...Execution Logs...

 +--------------------------------+---------------------------+
| Metric                         | Value                     |
+--------------------------------+---------------------------+
| Batch Start                    |                17,664,125 |
| Batch End                      |                17,664,250 |
| Execution Duration (seconds)   |                       606 |
| Total Instruction Count        |            11,055,051,645 |
| Oracle Verify Cycles           |               832,566,844 |
| Derivation Cycles              |             1,089,859,924 |
| Block Execution Cycles         |             8,959,507,779 |
| Blob Verification Cycles       |               338,156,173 |
| Total SP1 Gas                  |            13,075,527,707 |
| Number of Blocks               |                       126 |
| Number of Transactions         |                       856 |
| Ethereum Gas Used              |               416,711,464 |
| Cycles per Block               |                87,738,505 |
| Cycles per Transaction         |                12,914,779 |
| Transactions per Block         |                         6 |
| Gas Used per Block             |                 3,307,233 |
| Gas Used per Transaction       |                   486,812 |
+--------------------------------+---------------------------+
```
