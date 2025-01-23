# L2 Node Setup

This guide will show you how to set up an L2 execution node (`op-geth`) and a rollup node (`op-node`) for your OP Stack chain.

## Instructions
1. Clone [simple-optimism-node](https://github.com/smartcontracts/simple-optimism-node) and follow the instructions in the README to set up your rollup.
2. Ensure you configure the rollup with `NODE_TYPE`=`archive` and `OP_GETH__SYNCMODE`=snap. With this, you will be able to prove blocks in OP Succcinct using blocks after the snap sync.

Your `op-geth` endpoint will be available at the RPC port chosen [here](https://github.com/smartcontracts/simple-optimism-node/blob/main/scripts/start-op-geth.sh#L39), which in this case is `8545` (e.g. `http://localhost:8545`).

Your `op-node` endpoint (rollup node) will be available at the RPC port chosen [here](https://github.com/smartcontracts/simple-optimism-node/blob/main/scripts/start-op-node.sh#L21), which in this case is `9545` (e.g. `http://localhost:9545`).

## Check Sync Status

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
