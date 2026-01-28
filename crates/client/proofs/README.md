# base-proofs-extension

Proofs history extension for Base node. Enables on-disk storage and retrieval of historical Merkle proofs for state verification.

## Overview

This extension provides the infrastructure for maintaining a historical record of Merkle proofs, allowing clients to verify the state of the blockchain at any point within the configured retention window. It extends the node with an Execution Extension (ExEx) and additional RPC methods for proof retrieval.

## Features

- **On-disk proofs storage**: Uses MDBX for efficient storage of historical proofs
- **Configurable retention window**: Set how far back proofs are retained
- **Automatic pruning**: Periodically removes old proofs beyond the retention window
- **Proof verification**: Optionally verify stored proofs at configurable intervals
- **Metrics reporting**: Tracks storage size and performance metrics
- **Extended RPC API**: Adds `eth_getProof` and debug endpoints for proof retrieval

## Configuration

The extension is configured through `RollupArgs`:

- `proofs_history`: Enable/disable proofs history
- `proofs_history_storage_path`: Path to MDBX storage directory
- `proofs_history_window`: Number of blocks to retain proofs for
- `proofs_history_prune_interval`: How often to prune old proofs
- `proofs_history_verification_interval`: How often to verify stored proofs

## Usage

The extension is installed as part of the node builder and automatically:

1. Captures proofs during block execution
2. Stores them in the configured MDBX database
3. Serves proof requests via extended RPC methods
4. Prunes old proofs based on the retention window
5. Reports metrics for monitoring storage and performance
