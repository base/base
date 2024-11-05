# Block Data

The `block-data` script is a convenient CLI tool to fetch the block & fee data for a given range of blocks on a rollup.

> Performs better with high RPS (requests per second) supported on the L2 RPC endpoint.

## Overview

In the root directory, add the following RPC to your `.env` file for your rollup:

| Parameter | Description |
|-----------|-------------|
| `L2_RPC` | L2 Execution Node (`op-geth`). |

To perform analysis on the fees collected on L2, you can use the `block-data` script. This script will fetch the block & fee data for each block in the range from the L2 and output a CSV file with the columns: `block_number`, `transaction_count`, `gas_used`, `total_l1_fees`, `total_tx_fees`.

Compared to the cost estimator, the block data script is much faster and requires less resources, so it's recommended to use this script if you only need the block data and want to calculate data quantities like average txns per block, avg gas per block, etc.

Once the script has finished execution, it will write the statistics for each block in the range to a CSV file at `block-data/{chain_id}/{start_block}-{end_block}.csv`.

## Run the Block Data Script

To run the block data script, use the following command:

```shell
RUST_LOG=info cargo run --bin block-data --release -- --start <start_l2_block> --end <end_l2_block>
```

## Optional flags

| Flag | Description |
|-----------|-------------|
| `--env-file` | The path to the environment file to use. (Ex. `.env.opmainnet`) |

## Useful Commands

- `cast block finalized -f number --rpc-url <L2_RPC>`: Get the latest finalized block number on the L2.
- `cast bn --rpc-url <L2_RPC>`: Get the latest block number on the L2.

## Sample Output

### `stdout`

Fetching block data for blocks 5,484,100 to 5,484,200 on World Chain Mainnet:

```shell
Wrote block data to block-data/480/5484100-5484200-block-data.csv

Aggregate Block Data for blocks 5484100 to 5484200:
Total Blocks: 101
Total Transactions: 1977
Total Gas Used: 546370916
Total L1 Fees: 0.003644 ETH
Total TX Fees: 0.004038 ETH
Avg Txns/Block: 19.57
Avg Gas/Block: 5409613.03
Avg L1 Fees/Block: 0.000036 ETH
Avg TX Fees/Block: 0.000040 ETH
```

### csv

```csv
block_number,transaction_count,gas_used,total_l1_fees,total_tx_fees
5710000,13,5920560,8004099318905,9272809586600
5710001,10,3000975,4810655220218,5353583855906
5710002,16,7269303,10842878782866,12174517937838
5710003,10,4521953,6142429255728,6967734553050
5710004,10,5558505,6534408691877,7550749486247
5710005,14,6670097,10210757953683,11431964042061
5710006,7,4725805,5863003171921,6725878188991
5710007,10,4798495,7011790976814,8252141668373
5710008,7,3805639,4428577556414,5121866899850
5710009,9,4348521,6732491769192,7697076627632
5710010,17,8728999,12317431205317,14022097163617
5710011,9,5882229,6229718004537,7411261930653
5710012,8,2053460,3062348125908,3938363190799
5710013,11,4829936,7756633163332,8721805365111
5710014,5,798837,1684650453190,2292476216082
5710015,10,4123290,7122749550608,7874581655693
5710016,8,1416529,3110958598569,3414612717107
...
```
