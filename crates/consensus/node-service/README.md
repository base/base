# `base-node-service`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Unified rollup node service using kona actors with in-process engine.

## Overview

This crate provides [`UnifiedRollupNode`] which orchestrates kona's actors
(NetworkActor, DerivationActor, L1WatcherActor) with our custom engine actor
for direct in-process execution layer communication via channels.

- **`UnifiedRollupNode`**: Main node service that manages all actors.
- **`UnifiedRollupNodeBuilder`**: Builder pattern for constructing the node.

## Architecture

```text
┌────────────────────────────────────────────────────────────────────────┐
│  UnifiedRollupNode                                                      │
│                                                                         │
│  ┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐       │
│  │ NetworkActor    │   │ DerivationActor │   │ L1WatcherActor  │       │
│  │ (from kona)     │   │ (from kona)     │   │ (from kona)     │       │
│  └────────┬────────┘   └────────┬────────┘   └────────┬────────┘       │
│           │                     │                     │                │
│           └──────────┬──────────┴──────────┬──────────┘                │
│                      ▼                     ▼                            │
│              mpsc::channel          mpsc::channel                       │
│                      │                     │                            │
│  ┌───────────────────▼─────────────────────▼───────────────────┐       │
│  │ DirectEngineActor                                            │       │
│  │ └─ InProcessEngineDriver (channel-based, no HTTP/IPC)        │       │
│  └───────────────────┬─────────────────────────────────────────┘       │
│                      │                                                  │
│                      ▼ (in-process channel send)                       │
│  ┌─────────────────────────────────────────────────────────────┐       │
│  │ reth EL (EngineApiTreeHandler via ConsensusEngineHandle)     │       │
│  └─────────────────────────────────────────────────────────────┘       │
└────────────────────────────────────────────────────────────────────────┘
```

## Usage

```toml
[dependencies]
base-node-service = { git = "https://github.com/base/node-reth" }
```

Create and start the unified node:

```rust,ignore
use base_node_service::UnifiedRollupNode;
use base_engine_bridge::InProcessEngineDriver;
use std::sync::Arc;

// In reth's AddOns hook, you have access to the engine handles
let engine_driver = Arc::new(InProcessEngineDriver::new(
    engine_handle,  // from ctx.beacon_engine_handle
    payload_store,  // from PayloadStore::new(ctx.node.payload_builder_handle())
    provider,       // from ctx.node.provider()
));

let node = UnifiedRollupNode::builder()
    .config(Arc::new(rollup_config))
    .engine_driver(engine_driver)
    .build();

node.start().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/node-reth/blob/main/LICENSE).
