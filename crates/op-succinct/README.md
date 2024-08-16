# op-succinct

Standalone repo to use Kona & SP1 to verify OP Stack blocks.

## Overview

**`crates`**
- `client-utils`: A suite of utilities for the client program.
- `host-utils`: A suite of utilities for constructing the host which runs the OP Succinct program.

**`sp1-kona`**
- `native-host`: The host program which runs the `op-succinct` program natively using `kona`.
- `zkvm-host`: The host program which runs the `op-succinct` program in the SP1 zkVM.
- SP1 Programs
   - `zkvm-client`: Derive and execute a single block. First runs `zkvm-client` on the `native-host` to fetch the witness data, then uses SP1 to generate the program's proof of execution.
   - `validity-client`: Run the derivation pipeline & execute a batch of blocks. First runs `validity-client` on the `native-host` to fetch the witness data, then uses SP1 to generate the program's proof of execution.
   - `aggregation-client`: Aggregates validity proofs for a larger range of blocks.


## Usage

Execute the OP Succinct program for a single block.

```bash
just run-single <l2_block_num> [use-cache]
```

- [use-cache]: Optional flag to re-use the native execution cache (default: false).

Execute the OP Succinct program for a range of blocks.

```bash
just run-multi <start> <end> [use-cache] [prove]
```

- [use-cache]: Optional flag to re-use the native execution cache (default: false).
- [prove]: Optional flag to prove the execution (default: false).

Observations: 
* For most blocks, the cycle count per transaction is around 4M cycles per transaction.
* Some example cycle count estimates can be found [here](https://www.notion.so/succinctlabs/SP1-Kona-8b025f81f28f4d149eb4816db4e6d80b?pvs=4).

## Cycle Counts

To see how to get the cycle counts for a given block range, see [CYCLE_COUNT.md](./CYCLE_COUNT.md).


## Misc

To fetch an existing proof and save it run:

```bash
cargo run --bin fetch_and_save_proof --release -- --request-id <proofrequest_id> --start <start_block> --end <end_block>
```

Ex. `cargo run --bin fetch_and_save_proof --release -- --request-id proofrequest_01j4ze00ftfjpbd4zkf250qwey --start 123812410 --end 123812412`

## Run the OP Succinct Proposer

To run the OP Succinct Proposer to generate proofs for an OP Stack chain, see 
[OP_PROPOSER.md](./OP_PROPOSER.md).