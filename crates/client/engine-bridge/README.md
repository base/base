# `base-engine-bridge`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

In-process engine bridge for zero-overhead communication with reth's Engine API.

## Overview

This crate provides [`InProcessEngineDriver`] which implements the [`DirectEngineApi`]
trait by communicating with reth's Engine API directly via internal channels.

This approach provides:
- **Zero serialization**: Direct Rust type sharing, no JSON-RPC encoding
- **No network overhead**: Uses `ConsensusEngineHandle` channel, not HTTP/IPC
- **Lowest possible latency**: In-process function calls via tokio channels

## Components

- **`InProcessEngineDriver`**: Engine driver that communicates with reth via `ConsensusEngineHandle`.

## Architecture

```text
┌─────────────────┐     ┌─────────────────────┐     ┌─────────────────────────┐
│  Consensus      │     │ InProcessEngine     │     │  reth Engine            │
│  (kona-node)    │────▶│ Driver (channel)    │────▶│  (EngineApiTreeHandler) │
└─────────────────┘     └─────────────────────┘     └─────────────────────────┘
                              │                            │
                              │    tokio mpsc channel      │
                              └────────────────────────────┘
```

## Usage

```toml
[dependencies]
base-engine-bridge = { git = "https://github.com/base/node-reth" }
```

Create the driver within reth's AddOns hook where you have access to internal handles:

```rust,ignore
use base_engine_bridge::InProcessEngineDriver;
use std::sync::Arc;

// In reth's AddOns hook, you have access to internal handles
let driver = InProcessEngineDriver::new(
    ctx.beacon_engine_handle.clone(),  // ConsensusEngineHandle
    payload_store,                      // PayloadStore from PayloadBuilderHandle
    ctx.node.provider().clone(),        // Provider for block queries
);

// Use the DirectEngineApi methods - all via direct channel sends
let status = driver.new_payload_v3(payload, vec![], parent_root).await?;
let fcu = driver.fork_choice_updated_v3(state, Some(attributes)).await?;
let envelope = driver.get_payload_v3(payload_id).await?;
```

## How It Works

The driver uses three key reth internals:

1. **`ConsensusEngineHandle`**: Provides direct channel access to reth's `EngineApiTreeHandler`.
   Engine API calls like `new_payload` and `fork_choice_updated` are sent via this channel.

2. **`PayloadStore`**: Retrieves built payloads for `get_payload_*` methods without going
   through JSON-RPC.

3. **Provider**: Used for block queries (`l2_block_info_by_number`, etc.) via direct
   database access.

## Why In-Process Instead of HTTP/IPC?

| Approach | Latency | Serialization | Complexity |
|----------|---------|---------------|------------|
| HTTP     | ~1-5ms  | Full JSON-RPC | High       |
| IPC      | ~0.1ms  | Full JSON-RPC | Medium     |
| Channel  | ~1μs    | None          | Low        |

By using reth's internal channel API, we completely eliminate serialization overhead
and achieve the lowest possible latency for consensus-to-execution communication.

## License

Licensed under the [MIT License](https://github.com/base/node-reth/blob/main/LICENSE).
