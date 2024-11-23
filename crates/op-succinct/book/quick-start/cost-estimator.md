# Cost Estimator

The cost estimator is a convenient CLI tool to determine the costs of running OP Succinct. The tool simulates proving the validity of a range of blocks and returns the "costs" in terms of RISC-V cycles.

> Machines with fast network connectivity (500+ Mbps) are recommended because witness generation is bandwidth-intensive.

## Overview

In the root directory, add the following RPCs to your `.env` file for your rollup:

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L1_BEACON_RPC` | L1 Consensus (Beacon) Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |

More details on the RPC requirements can be found in the [prerequisites](../quick-start/prerequisites.md) section.

By running the cost estimator, you can validate that your endpoints are configured correctly.

## Running the Cost Estimator

### Overview

The cost estimator:
1. Splits large block ranges into smaller batches
2. Simulates proving each batch (similar to `op-succinct`)
3. Outputs aggregate statistics to a CSV.

> ⚠️ First run with a small block range - execution can be slow for large ranges.

### Basic Usage

Run for the last 5 finalized blocks:
```shell
RUST_LOG=info just cost-estimator
```

Run for a specific block range:
```shell
RUST_LOG=info just cost-estimator <start_l2_block> <end_l2_block>
```

### Configuration Options

| Flag | Default | Description |
|-----------|-------------|-------------|
| `batch-size` | 300 | Blocks per batch. Chain-specific defaults:<br>- Base: 5<br>- OP Mainnet: 10<br>- OP Sepolia: 30 |
| `env-file` | `.env` | Custom env file path (e.g. `.env.opmainnet`) |
| `use-cache` | false | Reuse previously generated witness data |

### Advanced Usage

Full command with all options:
```shell
RUST_LOG=info cargo run --bin cost-estimator --release \
    --start <start_l2_block> \
    --end <end_l2_block> \
    --env-file <path_to_env_file> \
    --batch-size <batch_size> \
    --use-cache
```

### Block Number Helpers

`cast` is a CLI tool installed with Foundry that can be used to fetch the latest/finalized block number of an OP Stack chain.


```shell
# Get latest finalized block
cast block finalized -f number --rpc-url <L2_RPC>

# Get latest block
cast bn --rpc-url <L2_RPC>
```

### Sample Output

#### `stdout`

Executing blocks 5,484,100 to 5,484,200 on World Chain Mainnet:

```shell
Aggregate Execution Stats for Chain 480: 
 +--------------------------------+---------------------------+
| Metric                         | Value                     |
+--------------------------------+---------------------------+
| Batch Start                    |                 5,484,100 |
| Batch End                      |                 5,484,200 |
| Witness Generation (seconds)   |                        66 |
| Execution Duration (seconds)   |                       458 |
| Total Instruction Count        |            19,707,995,043 |
| Oracle Verify Cycles           |             1,566,560,795 |
| Derivation Cycles              |             2,427,683,234 |
| Block Execution Cycles         |            15,442,479,993 |
| Blob Verification Cycles       |               674,091,948 |
| Total SP1 Gas                  |            22,520,841,820 |
| Number of Blocks               |                       101 |
| Number of Transactions         |                     1,977 |
| Ethereum Gas Used              |               546,370,916 |
| Cycles per Block               |               195,128,663 |
| Cycles per Transaction         |                 9,968,636 |
| Transactions per Block         |                        19 |
| Gas Used per Block             |                 5,409,613 |
| Gas Used per Transaction       |                   276,363 |
| BN Pair Cycles                 |             7,874,860,533 |
| BN Add Cycles                  |               310,550,754 |
| BN Mul Cycles                  |             1,636,223,094 |
| KZG Eval Cycles                |                         0 |
| EC Recover Cycles              |                96,983,901 |
+--------------------------------+---------------------------+
```

#### csv

The CSV associated with the range will have the columns from the `ExecutionStats` struct. The aggregate data for executing each "batch" within the block range will be included in the CSV.

Here, the CSV is `execution-reports/480/5484100-5484200.csv`:

```csv
batch_start,batch_end,witness_generation_time_sec,total_execution_time_sec,total_instruction_count,oracle_verify_instruction_count,derivation_instruction_count,block_execution_instruction_count,blob_verification_instruction_count,total_sp1_gas,nb_blocks,nb_transactions,eth_gas_used,l1_fees,total_tx_fees,cycles_per_block,cycles_per_transaction,transactions_per_block,gas_used_per_block,gas_used_per_transaction,bn_pair_cycles,bn_add_cycles,bn_mul_cycles,kzg_eval_cycles,ec_recover_cycles
5484184,5484200,0,0,2877585481,299740342,462008456,2066943417,134844448,3304337522,17,316,81926057,540908658541982,596950839845253,169269734,9106283,18,4819179,259259,1017572318,40106182,211873811,0,11948870
5484100,5484120,0,0,3754402331,308914395,461086557,2932162167,134779302,4287957207,21,350,106933244,710095197624994,783053876268826,178781063,10726863,16,5092059,305523,1561122615,61455811,324588766,0,14705709
5484163,5484183,0,0,4005997705,311171459,435265918,3206173584,134844448,4570091686,21,365,110883055,690170871678571,779801718014140,190761795,10975336,17,5280145,303789,1676711949,66155955,346844998,0,17337504
5484121,5484141,0,0,4152572226,316652487,486166806,3293854188,134779302,4746305028,21,440,117222955,767310117504021,846733606470274,197741534,9437664,20,5582045,266415,1584230563,62520537,329178548,0,25124445
5484142,5484162,0,0,4917437300,330082112,583155497,3943346637,134844448,5612150377,21,506,129405605,935016666707488,1031433531465147,234163680,9718255,24,6162171,255742,2035223088,80312269,423736971,0,27867373
```
