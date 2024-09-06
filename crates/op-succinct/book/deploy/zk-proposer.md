# Deploy ZK Proposer

After deploying the `ZKL2OutputOracle.sol`, the final step is to launch a "ZKP" version of the `op-proposer` that will call to [Succinct's Prover Network](TODO) to generate proofs and submit them onchain.

## Deployment

First, create a `.env` file that matches the `.env.example` file in the root of the `op-succinct` repo. 

```bash
cp .env.example .env
```

<!-- TODO: include more details about how to fill in the .env -->


Then, to run the OP proposer, follow these steps:

Build the server with:

```bash
docker compose build
```

Then, start the server:
```bash
docker compose up -d
```

To stop the server, run:
```bash
docker compose down
```

This launches a Docker container with a modified version of the `op-proposer` (which can be found at [our fork](https://github.com/succinctlabs/optimism/tree/zk-proposer)). 

The modified `op-proposer` performs the following tasks:
- Monitors L1 state to determine when to request a proof.
- Requests proofs from the Kona SP1 server.
- Once proofs have been generated for a sufficiently large range (specified by `SUBMISSION_INTERVAL` in `zkconfig.json`), aggregates batch proofs and submits them on-chain.


## Underneath the Hood

Underneath the hood, there are a very components involved in running the `op-succinct` proposer: 

**`op-succinct` Server**:
- There is a very lightweight server running with [op-succinct-proposer/bin/server.rs] that is is responsible or given a block range, it will generate the witness data for that block range by running the "native host" and then dispatch the proof request to the Succinct Prover Network.

**`op-succinct` Proposer**:
   - Runs the modified `op-proposer` binary from the Optimism repository which keeps the `ZKL2OutputOracle` contract up to date with the latest L2 state using
   SP1 proofs.
   - When a new L2 block is detected, the proposer requests a ZK proof from the OP Succinct Server. Once the proof is generated, it's
   posted to the `ZKL2OutputOracle` contract.
   - Uses a SQLite database (`proofs.db`) to keep track of processed blocks and proofs.
