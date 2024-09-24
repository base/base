# Proposer

Now that you have deployed the `OPSuccinctL2OutputOracle` contract, you can start the `op-succinct-proposer` service which replaces the normal `op-proposer` service in the OP Stack.

The `op-succinct-proposer` service will call to [Succinct's Prover Network](https://docs.succinct.xyz/generating-proofs/prover-network) to generate proofs of the execution and derivation of the L2 state transitions.

The modified proposer  performs the following tasks:
1. Monitors L1 state to determine when to request a proof.
2. Requests proofs from the OP Succinct server. The server sends requests to the Succinct Prover Network.
3. Once proofs have been generated for a sufficiently large range, aggregates range proofs and submits them on-chain.

We've packaged the `op-succinct-proposer` service in a docker-compose file to make it easier to run.

## 1) Build the Proposer Service

Build the docker images for the `op-succinct-proposer` service.

```bash
docker compose build
```

## 2) Run the Proposer

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