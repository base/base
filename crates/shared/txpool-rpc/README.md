# base-txpool-rpc

Transaction pool RPC extension for Base node. Enables transaction status queries and pool management.

## Overview
This extension provides the RPC modules `TransactionStatusApiImpl` and `TxPoolManagementApiImpl` for 
getting tranasaction status through the sequencer URL (if configured) or a local transaction pool.
It also extends the node with additional RPC methods for removing transaction(s), filtering by its hash,
sender or all transactions from its local transaction pool.


## Configuration

The extension is configured through `RollupArgs`:

- `sequencer`: Sequencer URL for forwarding state queries, otherwise default to local transaction pool

## Usage

The extension is installed as part of the node builder.