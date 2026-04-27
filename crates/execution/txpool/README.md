# `base-txpool`

Transaction pool for Base.

## Overview

Extends Reth's transaction pool with Base-specific validation and ordering for the Base node.
`BaseTransactionValidator` enforces L1 data fee checks and Base-specific validity rules.
`BaseOrdering` and `TimestampOrdering` provide customizable transaction prioritization strategies.
Also includes a `Consumer` for processing mempool events, a `Forwarder` for relaying transactions,
and a `BuilderApiImpl` for builder-specific pool management.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-txpool = { workspace = true }
```

```rust,ignore
use base_txpool::{BaseOrdering, BaseTransactionPool, BaseTransactionValidator};

let pool = Pool::new(
    BaseTransactionValidator::new(client, evm),
    BaseOrdering::default(),
    config,
);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
