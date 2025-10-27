# Experimental Features

This section covers experimental features for OP Succinct (Validity).

## Celestia DA

The `op-succinct-celestia` service monitors the state of an OP Stack chain with Celestia DA enabled, requests proofs from the [Succinct Prover Network](https://docs.succinct.xyz/docs/sp1/prover-network/intro) and submits them to the L1.

For detailed setup instructions, see the [Celestia DA](./celestia.md) section.

## EigenDA DA

The `op-succinct-eigenda` service monitors the state of an OP Stack chain with EigenDA enabled, uses an EigenDA Proxy to retrieve and validate blobs from DA certificates, requests proofs from the [Succinct Prover Network](https://docs.succinct.xyz/docs/sp1/prover-network/intro) and submits them to L1.

For detailed setup instructions, see the [EigenDA DA](./eigenda.md) section.
