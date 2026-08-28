# `base-tx-manager`

Transaction lifecycle management for Base onchain components.

## Overview

The crate constructs, signs, publishes, replaces, and confirms EIP-1559 and
EIP-4844 transactions. `SimpleTxManager` uses one coordinator task as the sole
owner of its signed pending ledger:

1. `submit` appends a candidate and returns a `SubmissionHandle` without doing
   RPC or signing work.
2. The coordinator assigns the next nonce, then a worker builds and signs that
   candidate.
3. The signed version is added to an ordered `VecDeque` ledger shared by every
   publication backend.
4. Each backend owns one sequential worker and cursor. Workers publish in nonce
   order, while different backends run concurrently.
5. An accepted or ambiguous response from any backend commits the nonce and
   allows construction of `n + 1`. A provisional nonce is recycled only after
   every backend definitively rejects it.
6. A fee bump atomically replaces the current version. Every backend rewinds to
   that nonce before publishing later entries again.
7. The chain reader confirms known hashes and removes only the resolved front
   prefix of the ledger.

A clean rejection may recycle a provisional nonce. A nonce is never recycled
after any publication attempt could have reached a provider.

## Public API

- `TxManager` — `submit`, `cancel_tx`, and `sender_address`.
- `SimpleTxManager` — default coordinator-backed implementation.
- `TxCandidate` — calldata, recipient, value, gas floor, and optional blobs.
- `SubmissionHandle` — cloneable lifecycle handle for observing or awaiting a
  submission.

Lower-level builder, coordinator, pending-ledger, publisher, sweeper, fee,
and error-classification types are also publicly re-exported for
workspace-wide reuse. Most consumers should use `SimpleTxManager` through the
`TxManager` trait.

## Publication backends

There is no primary publisher. The chain provider used for fee inputs, nonce
reads, receipts, and canonical confirmation is also included as publication
backend zero. Every additional publication provider is otherwise symmetric.
Construction fails if any configured backend's chain ID cannot be validated.
Backend progress is independent after startup.

The coordinator distributes the latest immutable ledger snapshot through a
`watch` channel per backend. A slow backend receives the newest snapshot
without accumulating obsolete versions. Workers report classified outcomes to
the coordinator through `mpsc`.

Within one backend, publication is FIFO: nonce `n + 1` is attempted only after
that backend accepted the current version of `n`. Across backends, publication
is parallel. One slow or unavailable backend does not stop another backend
from advancing.

## Error classification

The manager maps recognized node responses to explicit `TxManagerError`
variants. Publication uses a stricter internal classification:

- `AlreadyKnown` is accepted.
- Transport failures and unknown RPC responses are ambiguous.
- Recognized nonce, fee, reservation, and deterministic errors are clean
  rejections.

This distinction is what makes provisional nonce reuse safe.

## Configuration

`TxManagerConfig` controls confirmation depth, fee limits, network timeouts,
resubmission cadence, publication pass cadence, and receipt polling. Manager
construction validates it before network startup.

The `define_tx_manager_cli!` macro creates a clap argument group with environment
fallbacks:

```rust,ignore
base_tx_manager::define_tx_manager_cli!("MY_SERVICE");

#[derive(clap::Parser)]
struct Cli {
    #[command(flatten)]
    tx: TxManagerCli,
}

let config = TxManagerConfig::try_from(cli.tx)?;
```

Consumer crates using this macro must depend on `clap` with `derive` and `env`
features and on `humantime`.

## Usage

```rust,ignore
use std::sync::Arc;

use alloy_primitives::{Address, U256, bytes};
use base_tx_manager::{
    BaseTxMetrics, SignerConfig, SimpleTxManager, TxCandidate, TxManager, TxManagerConfig,
};

let manager = SimpleTxManager::new(
    provider,
    SignerConfig::local(signer),
    TxManagerConfig::default(),
    chain_id,
    Arc::new(BaseTxMetrics::new("my_service")),
)
.await?;

let candidate = TxCandidate {
    tx_data: bytes!("deadbeef"),
    to: Some(Address::ZERO),
    gas_limit: 21_000,
    value: U256::from(1_000),
    ..Default::default()
};

let handle = manager.submit(candidate);
let receipt = handle.wait().await?;
```

Applications that need a bounded wait can wrap `wait()` in their runtime's
timeout primitive. Dropping that wait does not cancel manager-owned nonce work.

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
