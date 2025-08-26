# Node Setup

## Overview

To run OP Succinct or OP Succinct Lite, you will need the following RPCs in your `.env`:

- `L1_RPC`: L1 Execution Archive Node
- `L1_BEACON_RPC`: L1 Consensus (Beacon) Node
- `L2_RPC`: L2 Execution Node (`op-geth`)
- `L2_NODE_RPC`: L2 Rollup Node (`op-node`)

<div class="warning">
When running the proposer in production, it is recommended that your L2 nodes are in the same region/network to minimize latency. Otherwise, tasks like witness generation can take significantly longer and become a bottleneck.
</div>

## Required Accessible Endpoints

The RPCs must support the following endpoints:

| RPC | Endpoints | Description |
|-----|-----------|-------------|
| `L1_RPC` | `debug_getRawHeader`, `debug_getRawReceipts`, `debug_getRawBlock` | L1 Execution Archive Node |
| `L2_RPC` | `debug_getRawHeader`, `debug_getRawTransaction`, `debug_getRawBlock`, `debug_dbGet` | L2 Execution Node (`op-geth`) |
| `L2_NODE_RPC` | `optimism_outputAtBlock`, `optimism_rollupConfig`, `optimism_syncStatus`, `optimism_safeHeadAtL1Block` | L2 Rollup Node (`op-node`) |

## External RPC Provider

First, we'd recommend asking your RPC provider if they support these endpoints. If they do, you can use them.

> **Note:** While some RPC providers (like Alchemy and Infura) support the required L1 endpoints after enabling them, most RPC providers do not support the `debug_dbGet` endpoint for external users. You will likely need to run your own L2 nodes for this reason.

## Running Your Own L2 Nodes

If you don't have access to these endpoints, you can run your own L2 nodes.

### Instructions
1. Clone [simple-optimism-node](https://github.com/smartcontracts/simple-optimism-node) and follow the instructions in the README to set up your rollup.
2. Ensure you configure the rollup with `NODE_TYPE`=`archive` and `OP_GETH__SYNCMODE`=snap. With this, you will be able to prove blocks in OP Succcinct using blocks after the snap sync.

Your `op-geth` endpoint will be available at the RPC port chosen [here](https://github.com/smartcontracts/simple-optimism-node/blob/main/scripts/start-op-geth.sh#L39), which in this case is `8545` (e.g. `http://localhost:8545`).

Your `op-node` endpoint (rollup node) will be available at the RPC port chosen [here](https://github.com/smartcontracts/simple-optimism-node/blob/main/scripts/start-op-node.sh#L21), which in this case is `9545` (e.g. `http://localhost:9545`).

#### Check Sync Status

After a few hours, your node should be fully synced and you can use it to begin generating ZKPs.

To check your node's sync status, you can run the following commands:

**op-geth:**

```bash
curl -H "Content-Type: application/json" -X POST --data '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}' http://localhost:8545
```

**op-node:**

```bash
curl -H "Content-Type: application/json" -X POST --data '{"jsonrpc":"2.0","method":"optimism_syncStatus","params":[],"id":1}' http://localhost:9545
```
