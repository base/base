# FAQ

## Has OP Succinct been audited?

Cantina has audited both [OP Succinct](https://github.com/succinctlabs/op-succinct/blob/edward/docs/audits/OP%20Succinct%20Spearbit.pdf) and [OP Succinct Lite](https://github.com/succinctlabs/op-succinct/blob/edward/docs/audits/OP%20Succinct%20Lite%20Spearbit.pdf). 

## How is data availability proven?

The `range` program proves the correctness of an OP Stack derivation + STF for a range of blocks. The `BlobProvider` verifies that the raw data (compressed L2 transaction calldata) matches the blob hash, and the `ChainProvider` verifies that the blob hashes belong to a certain L1 block hash. At this point, we've verified that compressed L2 transaction calldata is available against a specific L1 block. 

Because the `range` program can include an arbitrary number of blocks with blobs, we supply an `l1BlockHash` to the verifier. Within the `range` program, we verify that the blocks from which the blobs are extracted chain up to the `l1BlockHash`. This `l1BlockHash` is made accessible when verifying a proof via the `checkpointBlockHash` function.

## Is OP Succinct compatible with the Superchain's Standard Configuration?

If your rollup adopts OP Succinct, it will no longer be classified as a Standard Chain. Optimism currently considers ZK proofs to be an "experimental" feature, similar to alt-DA solutions and custom gas tokens. We are in regular discussions with Optimism, and we hope that this policy will evolve in the future.

