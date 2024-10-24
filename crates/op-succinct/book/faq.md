# FAQ

## How is data availability proven?

The `range` program proves the correctness of an OP Stack derivation + STF for a range of blocks. The `BlobProvider` verifies that the raw data (compressed L2 transaction calldata) matches the blob hash, and the `ChainProvider` verifies that the blob hashes belong to a certain L1 block hash. At this point, we've verified that compressed L2 transaction calldata is available against a specific L1 block. 

Because the `range` program can include an arbitrary number of blocks with blobs, we supply an `l1BlockHash` to the verifier. Within the `range` program, we verify that the blocks from which the blobs are extracted chain up to the `l1BlockHash`. This `l1BlockHash` is made accessible when verifying a proof via the `checkpointBlockHash` function.
