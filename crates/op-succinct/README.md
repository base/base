# kona-sp1

Standalone repo to use Kona & SP1 to verify Optimism blocks.

## Overview

**`crates`**
- `client-utils`: A suite of utilities for the client program.
- `host-utils`: A suite of utilities for constructing the host which runs the SP1 Kona program.

**`sp1-kona`**
- `native-host`: The host program which runs the Kona program natively using `kona`.
- `zkvm-host`: The host program which runs the Kona program in SP1.
- `zkvm-client`: The program proven in SP1. The `zkvm-host` first runs `zkvm-client` on the `kona-host` to fetch the witness data, then uses SP1 to generate the program's proof of execution.

## Usage

Execute the SP1 Kona program for a single block.

```bash
just run-single <l2_block_num> [use-cache]
```

- [use-cache]: Optional flag to re-use the native execution cache (default: false).

Execute the SP1 Kona program for a range of blocks.

```bash
just run-multi <start> <end> [use-cache] [prove]
```

- [use-cache]: Optional flag to re-use the native execution cache (default: false).
- [prove]: Optional flag to prove the execution (default: false).

Observations: 
* For most blocks, the cycle count per transaction is around 4M cycles per transaction.
* Some example cycle count estimates can be found [here](https://www.notion.so/succinctlabs/SP1-Kona-8b025f81f28f4d149eb4816db4e6d80b?pvs=4).


## Misc

To fetch an existing proof and save it run:

```bash
cargo run --bin fetch_and_save_proof --release -- --request-id <proofrequest_id> --start <start_block> --end <end_block>
```

Ex. `cargo run --bin fetch_and_save_proof --release -- --request-id proofrequest_01j4ze00ftfjpbd4zkf250qwey --start 123812410 --end 123812412`