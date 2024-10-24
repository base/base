# L2 Node Setup

This guide will show you how to set up an L2 execution node (`op-geth`) and a rollup node (`op-node`) for your OP Stack chain.

## Instructions
1. Clone [ops-anton](https://github.com/anton-rs/ops-anton) and follow the instructions in the README to set up your rollup.
2. Go to [op-node.sh](https://github.com/anton-rs/ops-anton/blob/main/L2/op-mainnet/op-node/op-node.sh#L4-L6) and set the `L2_RPC` to your rollup RPC. Modify the `l1` and `l1.beacon` to your L1 and L1 Beacon RPCs. Note: Your L1 node should be an archive node.
3. If you are starting a node for a different chain, you will need to modify `op-network` in `op-geth.sh` [here](https://github.com/anton-rs/ops-anton/blob/main/L2/op-mainnet/op-geth/op-geth.sh#L18) and `network` in `op-node.sh` [here](https://github.com/anton-rs/ops-anton/blob/main/L2/op-mainnet/op-node/op-node.sh#L10).
4. In `/L2/op-mainnet` (or the directory you chose):
   1. Generate a JWT secret `./generate_jwt.sh`
   2. `docker network create anton-net` (Creates a Docker network for the nodes to communicate on).
   3. `just up` (Starts all the services).

Your `op-geth` endpoint will be available at the RPC port chosen [here](https://github.com/anton-rs/ops-anton/blob/main/L2/op-mainnet/op-geth/op-geth.sh#L7), which in this case is `8547` (e.g. `http://localhost:8547`).

Your `op-node` endpoint (rollup node) will be available at the RPC port chosen [here](https://github.com/anton-rs/ops-anton/blob/main/L2/op-mainnet/op-node/op-node.sh#L13), which in this case is `5058` (e.g. `http://localhost:5058`).

## Check Sync Status

After a few hours, your node should be fully synced and you can use it to begin generating ZKPs.

To check your node's sync status, you can run the following commands:

**op-geth:**

```bash
curl -H "Content-Type: application/json" -X POST --data '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}' http://localhost:8547
```

**op-node:**

```bash
curl -H "Content-Type: application/json" -X POST --data '{"jsonrpc":"2.0","method":"optimism_syncStatus","params":[],"id":1}' http://localhost:5058
```