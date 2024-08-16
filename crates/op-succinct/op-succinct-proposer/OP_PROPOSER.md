# Run OP Proposer

## Instructions 

Once you've deployed an `ZKL2OutputOracle` contract, you can run the OP Proposer to generate proofs.

First, create a `.env.server` file that matches the `.env.server.example` file.

```bash
cp .env.server.example .env.server
```

To run the OP Proposer, first build the server:
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


## Under the Hood

**`op-succinct` Server**:
   - Handles the ZK-proof generation and verification.

**`op-succinct` Proposer**:
   - Runs the modified `op-proposer` binary from the Optimism repository which keeps the `ZKL2OutputOracle` contract up to date with the latest L2 state using
   SP1 proofs.
   - When a new L2 block is detected, the proposer requests a ZK proof from the OP Succinct Server. Once the proof is generated, it's
   posted to the `ZKL2OutputOracle` contract.
   - Uses a SQLite database (`proofs.db`) to keep track of processed blocks and proofs.
