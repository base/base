# Proposer

Now that you have deployed the `OPSuccinctL2OutputOracle` contract, you can start the `op-succinct-proposer` service which replaces the normal `op-proposer` service in the OP Stack.

The `op-succinct-proposer` service will call to [Succinct's Prover Network](https://docs.succinct.xyz/generating-proofs/prover-network) to generate proofs of the execution and derivation of the L2 state transitions.

The modified proposer  performs the following tasks:
1. Monitors L1 state to determine when to request a proof.
2. Requests proofs from the OP Succinct server. The server sends requests to the Succinct Prover Network.
3. Once proofs have been generated for a sufficiently large range, aggregates range proofs and submits them on-chain.

We've packaged the `op-succinct-proposer` service in a docker-compose file to make it easier to run.

## 1) Environment Setup

Before starting the proposer, the following environment variables should be in your `.env` file. You should have already set up your environment when you deployed the L2 Output Oracle. If you have not done so, follow the steps in the [L2 Output Oracle](./l2-output-oracle.md) section.

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L1_BEACON_RPC` | L1 Consensus (Beacon) Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |
| `PROVER_NETWORK_RPC` | Default: `rpc.succinct.xyz`. |
| `SP1_PRIVATE_KEY` | Key for the Succinct Prover Network. Get access [here](https://docs.succinct.xyz/generating-proofs/prover-network). |
| `SP1_PROVER` | Default: `network`. Set to `network` to use the Succinct Prover Network. |
| `PRIVATE_KEY` | Private key for the account that will be deploying the contract and posting output roots to L1. |
| `L2OO_ADDRESS` | Address of the `OPSuccinctL2OutputOracle` contract. |

## 2) Build the Proposer Service

Build the docker images for the `op-succinct-proposer` service.

```bash
docker compose build
```

## 3) Run the Proposer

This command launches the `op-succinct-proposer` service in the background. It launches two containers: one container that manages proof generation and another container that is a small fork of the original `op-proposer` service.

After a few minutes, you should see the `op-succinct-proposer` service start to generate range proofs. Once enough range proofs have been generated, they will be verified in an aggregate proof and submitted to the L1.

```bash
docker compose up
```

To see the logs of the `op-succinct-proposer` service, run:

```bash
docker compose logs -f
```

and to stop the `op-succinct-proposer` service, run:

```bash
docker compose down
```