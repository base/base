# `base-execution-rpc`

RPC extensions for the Base execution node.

## Overview

Provides JSON-RPC API implementations for Base chains, including the full Ethereum API
(`OpEthApi`), the Engine API (`OpEngineApi`), debug, miner, sequencer, and witness endpoints.
Also includes `SequencerClient` for forwarding transactions to the sequencer and error types for
Base-specific RPC failures.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-rpc = { workspace = true }
```

```rust,ignore
use base_execution_rpc::{OpEthApiBuilder, SequencerClient};

let eth_api = OpEthApiBuilder::new()
    .with_sequencer(SequencerClient::new(sequencer_url))
    .build(ctx)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
