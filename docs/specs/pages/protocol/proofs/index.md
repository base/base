# Proofs

The proof system is the set of offchain services and onchain contracts that make L2 checkpoint
proposals verifiable from Ethereum. A proposal claims an output root for a fixed L2 block range.
Independent proof actors recompute that claim, provide proof material, and dispute the game if the
claim is invalid.

This section describes the component roles used by the Azul proof system.

- [Challenger](./challenger): checks in-progress games against canonical L2 state and disputes
  invalid claims.
- [Proposer](./proposer): creates new checkpoint proposals.
- [Registrar](./registrar): maintains the onchain registry of accepted TEE signer identities.
- [TEE Provers](./tee-provers): produce Nitro Enclave-backed proofs for the common proposal path.
- [ZK Provers](./zk-provers): produce permissionless proofs for proposal and dispute paths.
- [Contracts](./contracts): verify proof material, track game state, and release withdrawals and
  bonds according to the game result.

The legacy interactive fault-proof design is specified separately in [Fault Proofs](/protocol/fault-proof).
