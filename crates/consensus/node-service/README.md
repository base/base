# `base-node-service`

Base node service implementing the OP Stack rollup node.

## Overview

This crate provides [`BaseNode`] and [`BaseNodeBuilder`] which wrap kona's rollup node
infrastructure with Base-specific naming and configuration.

## Usage

```toml
[dependencies]
base-node-service = { git = "https://github.com/base/base" }
```

```rust,ignore
use base_node_service::{BaseNodeBuilder, L1ConfigBuilder, BaseEngineConfig};

let l1_config = L1ConfigBuilder {
    chain_config: l1_chain_config,
    trust_rpc: false,
    beacon: beacon_url,
    rpc_url: l1_rpc_url,
    slot_duration_override: None,
};

let node = BaseNodeBuilder::new(
    rollup_config,
    l1_config,
    l2_trust_rpc,
    engine_config,
    p2p_config,
    rpc_config,
)
.with_sequencer_config(sequencer_config)
.build();

node.start().await?;
```

## Architecture

The [`BaseNode`] orchestrates kona's actors (NetworkActor, DerivationActor, L1WatcherActor,
EngineActor) to implement the full OP Stack rollup node:

```text
┌────────────────────────────────────────────────────────────────────────┐
│  BaseNode                                                               │
│                                                                         │
│  ┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐       │
│  │ NetworkActor    │   │ DerivationActor │   │ L1WatcherActor  │       │
│  │ (P2P gossip)    │   │ (derives L2     │   │ (L1 finality)   │       │
│  │                 │   │  from L1 data)  │   │                 │       │
│  └────────┬────────┘   └────────┬────────┘   └────────┬────────┘       │
│           │                     │                     │                │
│           └──────────┬──────────┴──────────┬──────────┘                │
│                      ▼                     ▼                           │
│              mpsc::channel          mpsc::channel                      │
│                      │                     │                           │
│  ┌───────────────────▼─────────────────────▼───────────────────┐       │
│  │ EngineActor                                                  │       │
│  │ └─ OpEngineClient (RPC-based Engine API)                     │       │
│  └───────────────────┬─────────────────────────────────────────┘       │
│                      │                                                  │
│                      ▼ (Engine API RPC)                                │
│  ┌─────────────────────────────────────────────────────────────┐       │
│  │ L2 Execution Layer (reth)                                    │       │
│  └─────────────────────────────────────────────────────────────┘       │
└────────────────────────────────────────────────────────────────────────┘
```

## Re-exports

This crate re-exports the following types:

**Base types:**
- [`BaseNode`] - The Base rollup node service
- [`BaseNodeBuilder`] - Builder for constructing the node
- [`L1Config`] - L1 chain configuration
- [`L1ConfigBuilder`] - Builder for L1 configuration

**From kona-node-service:**
- [`NodeMode`] - Node operation mode (Validator, Sequencer)
- [`NetworkConfig`] - P2P network configuration
- [`SequencerConfig`] - Sequencer configuration
- [`InteropMode`] - Interop mode configuration

**From kona-genesis:**
- [`RollupConfig`] - Rollup configuration

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
