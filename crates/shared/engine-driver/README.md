# `base-engine-driver`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Transport-agnostic engine driver trait for direct execution layer communication.

## Overview

This crate defines the [`DirectEngineApi`] trait, which provides a transport-agnostic
interface for communicating with the execution layer. This enables the consensus layer
to drive the execution layer directly without going through the HTTP Engine API.

- **`DirectEngineApi`**: Core trait defining all Engine API operations (new_payload, fork_choice_updated, get_payload, block queries).
- **`DirectEngineError`**: Error type for engine driver operations.

## Why This Exists

The standard Engine API uses HTTP/JSON-RPC for communication between the consensus
layer (CL) and execution layer (EL). When running both layers in the same process,
this HTTP boundary adds unnecessary latency and complexity.

This trait allows implementations to:
- Communicate directly via in-process channels
- Eliminate serialization/deserialization overhead
- Share types directly between CL and EL

## Usage

```toml
[dependencies]
base-engine-driver = { git = "https://github.com/base/node-reth" }
```

Implement the trait for your driver:

```rust,ignore
use base_engine_driver::{DirectEngineApi, DirectEngineError};
use async_trait::async_trait;

struct MyEngineDriver { /* ... */ }

#[async_trait]
impl DirectEngineApi for MyEngineDriver {
    type Error = DirectEngineError;

    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> Result<PayloadStatus, Self::Error> {
        // Direct in-process communication
    }
    // ... other methods
}
```

## License

Licensed under the [MIT License](https://github.com/base/node-reth/blob/main/LICENSE).
