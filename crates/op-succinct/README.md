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
just run-single <l2_block_num> [verbosity] [use-cache]
```

Execute the SP1 Kona program for a range of blocks.

```bash
just run-multi <start> <end> [verbosity] [use-cache]
```

- [verbosity]: Optional verbosity level (default: 0).
- [use-cache]: Optional flag to re-use the native execution cache (default: false).

Observations: 
* For most blocks, the cycle count per transaction is around 4M cycles per transaction.
* TODO: Do further analysis on the cycle count.
