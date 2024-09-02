# Proposer

Now that you have deployed the `ZKL2OutputOracle` contract, you can start the `op-succinct-proposer` service which replaces the normal `op-proposer` service in the OP Stack.

The `op-succinct-proposer` service will call to [Succinct's Prover Network](https://google.com) to generate proofs of the execution and derivation of the L2 state transitions.

The modified proposer  performs the following tasks:
1. Monitors L1 state to determine when to request a proof.
2. Requests proofs from the OP Succinct server.
3. Once proofs have been generated for a sufficiently large range, aggregates batch proofs and submits them on-chain.

We've packaged the `op-succinct-proposer` service in a docker-compose file to make it easier to run.

## 1) Build the Proposer

Build the docker images for the `op-succinct-proposer` service.

```bash
cd proposer
sudo docker-compose build
```

## 2) Run the Proposer

This command launches the `op-succinct-proposer` service in the background. It launches two containers: one container that manages proof generation and another container that is a small fork of the original `op-proposer` service.

```bash
sudo docker-compose up
```

To see the logs of the `op-succinct-proposer` service, run:

```bash
sudo docker-compose logs -f
```

and to stop the `op-succinct-proposer` service, run:

```bash
sudo docker-compose down
```