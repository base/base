# Modifications to Original `L2OutputOracle`

The original `L2OutputOracle` contract can be found [here](https://github.com/ethereum-optimism/optimism/blob/d356d92a33aa623e30e1e11435ec0c02da69d718/packages/contracts-bedrock/src/L1/L2OutputOracle.sol).

The changes introduced in the `OPSuccinctL2OutputOracle` contract are:

1. The `submissionInterval` parameter is now the minimum interval in L2 blocks at which checkpoints must be submitted. An aggregation proof can be posted after this interval has passed.
2. The addition of the `aggregationVkey`, `rangeVkeyCommitment`, `verifier`, `startingOutputRoot`, and `rollupConfigHash` parameters. `startingOutputRoot` is used for initializing the contract from an empty state, because `op-succinct` requires a starting output root from which to prove the next state root. The other parameters are used for verifying the proofs posted to the contract.
3. The addition of `historicBlockHashes` to store the L1 block hashes which the `op-succinct` proofs are anchored to. Whenever a proof is posted, the merkle proof verification will use these L1 block hashes to verify the state of the L2 which is posted as blobs or calldata to the L1.
4. The new `checkpointBlockHash` function which checkpoints the L1 block hash at a given L1 block number using the `blockhash` function.
5. The `proposeL2Output` function takes a `_proof` parameter, which is the proof that is posted to the contract, removes the unnecessary `_l1BlockHash` parameter (which is redundant given the `historicBlockHashes` mapping) and adds a `_proverAddress` parameter, which is the address of the prover that submitted the proof. This function also verifies the proof using the `ISP1VerifierGateway` contract.
6. Use an `initializerVersion` constant to track the "upgrade", rather than just the semantic version of the `OPSuccinctL2OutputOracle` contract.
