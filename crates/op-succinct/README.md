# op-succinct

Standalone repo to use Kona & SP1 to verify OP Stack blocks.


## ⚠️ Work in Progress

**Warning**: This repository is currently a work in progress. The code and documentation are actively being developed and may be subject to significant changes. Use with caution and expect frequent updates.


## Overview

**`crates`**
- `client-utils`: A suite of utilities for the client program.
- `host-utils`: A suite of utilities for constructing the host which runs the OP Succinct program.

**`op-succinct`**
- `native-host`: The host program which runs the `op-succinct` program natively using `kona`.
- `zkvm-host`: The host program which runs the `op-succinct` program in the SP1 zkVM.
- `programs`: The programs proven in SP1.
    - `fault-proof` and `range` are used to verifiably derive and execute single blocks
    and batches of blocks respectively. Their binary's are first run in native mode on the `kona-host` to
    fetch the witness data, then they use SP1 to verifiably execute the program.
   - For `aggregation`, which is used to generate an aggregate proof for a set of batches,
   first generate proofs for `range` programs for each batch, then use `aggregation` to
   generate an aggregate proof.

## Running `op-succinct`

For instructions on how to upgrade an OP Stack chain to use ZK validity proofs, refer to the [`op-succinct` Guide](./op-succinct-proposer/TUTORIAL.md).

## Estimating Cycle Counts

To learn how to estimate cycle counts for a given block range, check out our [Cycle Count Guide](./zkvm-host/CYCLE_COUNT.md).

## Open Source

This code is open sourced under the [Apache 2.0 License](./LICENSE.txt).
