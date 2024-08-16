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

When you run the OP Proposer, the following processes occur:

1. **Docker Compose Setup**: 
   - The `docker-compose.yml` file defines two services: `op-succinct-server` and `op-succinct-proposer`.
   - Both services use environment variables from the `.env.server` file.

2. **OP Succinct Server**:
   - Built using the `Dockerfile.server`.
   - Runs on port 3000.
   - This server handles the ZK-proof generation and verification.

3. **OP Succinct Proposer**:
   - Built using the `Dockerfile.op_proposer`.
   - Depends on the `op-succinct-server` service.
   - This service runs the modified `op-proposer` binary from the Optimism repository.

4. **OP Proposer Execution**:
   - The `op-proposer` binary is executed via the `op_proposer.sh` script.
   - It connects to the L1 and L2 nodes specified in the environment variables.
   - Monitors for new L2 blocks and generates ZK proofs for them.
   - Interacts with the `ZKL2OutputOracle` contract to submit new output proposals.

5. **Proof Generation**:
   - When a new L2 block is detected, the proposer requests a ZK proof from the OP Succinct Server.
   - The server generates the proof using the SP1 (Succinct Proofs of Execution) system.
   - Proof generation involves executing the L2 block in a ZK-friendly environment and creating a succinct proof of correct execution.

6. **Proposal Submission**:
   - Once a proof is generated, the OP Proposer submits a transaction to the L1 network.
   - This transaction calls the `proposeL2Output` function on the `ZKL2OutputOracle` contract.
   - The proposal includes the L2 output root and the ZK proof.

7. **Database Management**:
   - The proposer uses a SQLite database (`proofs.db`) to keep track of processed blocks and submitted proposals.
   - This ensures continuity across restarts and prevents duplicate submissions.

By running these services, you're effectively operating a bridge between your L2 network and the L1, ensuring that L2 state transitions are verifiably communicated to the L1 chain in a trustless manner.


