# kona-sp1

Standalone repo to use Kona & SP1 to verify Optimism blocks.

## Usage

```bash
just run [l2_block_num]
```

## Run the Kona SP1 program from CLI

Supply the L1 head, L2 output root, L2 claim, L2 claim block number, and chain ID.

```rust
cd zkvm-host

cargo run --release -- --l1-head bb3c26e67fd8acb1a2baa15cd9affc57347f8549775657537d2f2ae359384ba4 --l2-output-root 91c0ff7cdc5b59ff251b1c137b1f46c4c27e2b9f2ab17bb3b31c63d2f792a0a0 --l2-claim bfbec731f443c09bbfdcef53358458644ac2cbe1c5f68e53ad38599a52d65b5b --l2-claim-block 121866428 --chain-id 10
```