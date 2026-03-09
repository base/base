# `mempool-rebroadcaster`

Mempool rebroadcaster library.

## Overview

Subscribes to mempool state changes and rebroadcasts pending transactions to network peers.
`Rebroadcaster` processes `TxpoolDiff` events — tracking new arrivals and removals — and
forwards transactions that should be propagated, ensuring they reach all connected nodes even
when initial gossip is incomplete.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
mempool-rebroadcaster = { workspace = true }
```

```rust,ignore
use mempool_rebroadcaster::Rebroadcaster;

let rebroadcaster = Rebroadcaster::new(peers, pool);
rebroadcaster.run().await;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
