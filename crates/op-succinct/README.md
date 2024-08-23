# op-succinct

Standalone repo to use Kona & SP1 to verify OP Stack blocks.

## Overview

**`crates`**
- `client-utils`: A suite of utilities for the client program.
- `host-utils`: A suite of utilities for constructing the host which runs the OP Succinct program.

**`sp1-kona`**
- `native-host`: The host program which runs the `op-succinct` program natively using `kona`.
- `zkvm-host`: The host program which runs the `op-succinct` program in the SP1 zkVM.
- `client-programs`: The programs proven in SP1.
    - `fault-proof` and `range` are used to verifiably derive and execute single blocks
    and batches of blocks respectively. Their binary's are first run in native mode on the `kona-host` to
    fetch the witness data, then they use SP1 to verifiably execute the program.
   - For `aggregation`, which is used to generate an aggregate proof for a set of batches,
   first generate proofs for `range` programs for each batch, then use `aggregation` to
   generate an aggregate proof.

## Running `op-succinct`

For instructions on generating validity proofs for an OP Stack chain, refer to the [`op-succinct` Guide](./op-succinct-proposer/OP_PROPOSER.md).

## Estimating Cycle Counts

To learn how to estimate cycle counts for a given block range, check out our [Cycle Count Guide](./zkvm-host/CYCLE_COUNT.md).

## Open Source

This code is open sourced under the [Apache 2.0 License](./LICENSE.txt).
