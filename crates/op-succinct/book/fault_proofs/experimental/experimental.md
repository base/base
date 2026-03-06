# Experimental Features

This section covers experimental features for OP Succinct Lite.

## Celestia DA

The `op-succinct-lite-proposer-celestia` service monitors the state of an OP Stack chain with Celestia DA enabled, creates dispute games and requests proofs from the [Succinct Prover Network](https://docs.succinct.xyz/docs/sp1/prover-network/intro) and submits them to the L1.

For detailed setup instructions, see the [Celestia DA](./celestia.md) guide.

## EigenDA DA

The `op-succinct-lite-proposer-eigenda` service monitors the state of an OP Stack chain with EigenDA enabled, uses an EigenDA Proxy to retrieve and validate blobs from DA certificates, creates dispute games, requests proofs from the [Succinct Prover Network](https://docs.succinct.xyz/docs/sp1/prover-network/intro), and submits them to L1.

For detailed setup instructions, see the [EigenDA DA](./eigenda.md) guide.
