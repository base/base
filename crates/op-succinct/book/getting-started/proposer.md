# Proposer

Now that you have deployed the `ZKL2OutputOracle` contract, you can start the `op-succinct-proposer` service which replaces the normal `op-proposer` service in the OP Stack.

The `op-succinct-proposer` service will call to [Succinct's Prover Network](https://docs.succinct.xyz/generating-proofs/prover-network) to generate proofs of the execution and derivation of the L2 state transitions.

The modified proposer  performs the following tasks:
1. Monitors L1 state to determine when to request a proof.
2. Requests proofs from the OP Succinct server. The server sends requests to the Succinct Prover Network.
3. Once proofs have been generated for a sufficiently large range, aggregates span proofs and submits them on-chain.

We've packaged the `op-succinct-proposer` service in a docker-compose file to make it easier to run.

## 1) Set Proposer Parameters

In the root directory, create a file called `.env` (mirroring `.env.example`) and set the following environment variables:

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | The RPC URL for the L1 Ethereum node. |
| `L1_BEACON_RPC` | The RPC URL for the L1 Ethereum consensus node. |
| `L2_RPC` | The RPC URL for the L2 archive node (OP-Geth). |
| `L2_NODE_RPC` | The RPC URL for the L2 node. |
| `POLL_INTERVAL` | The interval at which to poll for new L2 blocks. |
| `L2OO_ADDRESS` | The address of the L2OutputOracle contract. |
| `PRIVATE_KEY` | The private key for the `op-proposer` account. |
| `L2_CHAIN_ID` | The chain ID of the L2 network. |
| `MAX_CONCURRENT_PROOF_REQUESTS` | The maximum number of concurrent proof requests (default is 20). |
| `MAX_BLOCK_RANGE_PER_SPAN_PROOF` | The maximum block range per span proof (default is 30). |
| `OP_SUCCINCT_SERVER_URL` | The URL of the OP Succinct server (default is http://op-succinct-server:3000). |
| `PROVER_NETWORK_RPC` | The RPC URL for the Succinct Prover Network. |
| `SP1_PRIVATE_KEY` | The private key for the SP1 account. |
| `SP1_PROVER` | The type of prover to use (set to "network"). |
| `SKIP_SIMULATION` | Whether to skip simulation of the proof before sending to the SP1 server (default is true). |
| `USE_CACHED_DB` | Whether to use a cached database for the proposer (default is false). If set to true, the DB is persisted between runs, and can be re-used after the proposer shuts down. |


## 2) Build the Proposer

Build the docker images for the `op-succinct-proposer` service.

```bash
docker-compose build
```

## 3) Run the Proposer

This command launches the `op-succinct-proposer` service in the background. It launches two containers: one container that manages proof generation and another container that is a small fork of the original `op-proposer` service.

After a few minutes, you should see the `op-succinct-proposer` service start to generate span proofs. Once enough span proofs have been generated, they will be verified in an aggregate proof and submitted to the L1.

```bash
docker-compose up
```

To see the logs of the `op-succinct-proposer` service, run:

```bash
docker-compose logs -f
```

and to stop the `op-succinct-proposer` service, run:

```bash
docker-compose down
```