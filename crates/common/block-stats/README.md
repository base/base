# `base-block-stats`

Per-block statistics derived from a block header and its transaction list.

## Overview

`BlockStats` computes the figures a Base block producer and the offline shadow-block
reader both report per block: total gas used, total transaction count, non-deposit
transaction count, and priority-fee ordering inversions. Centralizing the derivation
keeps the live builder emitter and the persisted-row reader from drifting on the
invariants that define these figures — deposits are excluded from the fee-ordered
vector, transactions with no effective tip are skipped rather than treated as zero,
and an inversion is a strict `next > previous` increase in adjacent effective tips.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-block-stats = { workspace = true }
```

```rust,ignore
use base_block_stats::BlockStats;

let stats = BlockStats::from_transactions(
    header.gas_used,
    header.base_fee_per_gas.unwrap_or_default(),
    &block.body().transactions,
);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
