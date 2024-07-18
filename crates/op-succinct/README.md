# kona-sp1

Standalone repo to use Kona & SP1 to verify Optimism blocks.

## Usage

To run the program in witness gen mode, then use that witness to create a proof in the ZKVM:

```bash
just run [l2_block_num]
```

If you already have witness data, run the program in the ZKVM by passing the L1 head, L2 output root, L2 claim, L2 claim block number, and chain ID:

```bash
just run-zkvm-host \
bb3c26e67fd8acb1a2baa15cd9affc57347f8549775657537d2f2ae359384ba4 \
91c0ff7cdc5b59ff251b1c137b1f46c4c27e2b9f2ab17bb3b31c63d2f792a0a0 \
bfbec731f443c09bbfdcef53358458644ac2cbe1c5f68e53ad38599a52d65b5b \
121866428 \
10
```

## TODO
Some blocks fail due to an OOM in SP1. We should debug this and see if either 1) there is an infinite loop causing OOM, 2) we should modify the SP1 Kona program to use less memory or 3) we should increase the heap memory usage in SP1.

- 122858451
