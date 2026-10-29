# ZK prover glossary

This glossary defines the proof requests, witnesses, and proving settings used by the ZK prover host and its Succinct SP1 backends.

- **Range proof**: A compressed SP1 proof of L2 execution after the starting block through the range end, inclusive. The range end is the starting block number plus the requested block count.
- **Aggregation proof**: A PLONK proof that verifies a completed compressed range proof and binds its public values to an L1 head and prover address. The current backends aggregate one range proof.
- **Range witness**: The execution inputs collected by the Succinct host for a range proof, encoded as `SP1Stdin` for the range program.
- **Intermediate block interval**: The number of L2 blocks between intermediate output roots of the game being proven. The request carries a nonzero interval and requires the block count to be a multiple of it. This is not a checkpoint interval.
- **L1 head**: The L1 block hash supplied to witness generation. A pinned head uses the request's exact hash. Without a pinned head, the provider calculates the head from the range end's L1 origin plus the sequence window, capped at the latest finalized L1 block.
- **Sequence window**: The number of L1 blocks added to the range end's L1 origin when calculating the L1 head. The request can override the provider's configured default. A pinned L1 head bypasses this calculation.
- **ZK proof request**: A `ZkProofRequest` containing the starting block number, block count, optional L1 head and sequence window, optional schedule block, intermediate block interval, VM, and backend.
- **Schedule block**: The optional L2 block whose timestamp selects the activated upgrade schedule. The guest uses the range end when this field is absent or zero.
- **Witness provider**: `OpSuccinctWitnessProvider`, which resolves a ZK proof request and builds its range witness. It owns the default sequence window and the RPC endpoints used to calculate the L1 head. It also builds aggregation witnesses.
- **Backend**: The execution target that consumes the witness. The cluster and Network backends submit SP1 proofs; the dry-run backend executes the range program locally and returns execution statistics without proof bytes.
