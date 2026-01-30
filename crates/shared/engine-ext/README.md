# base-engine-ext

In-process engine client for direct reth communication.

## Overview

This crate provides `InProcessEngineClient`, which bypasses the RPC layer
to communicate directly with reth's consensus engine via channels.

## Architecture

```text
┌─────────────────┐     ┌──────────────────────────┐     ┌─────────────────────────┐
│  Consensus      │     │ InProcessEngineClient    │     │  reth Engine            │
│  (kona-node)    │────▶│ (channel-based)          │────▶│  (EngineApiTreeHandler) │
└─────────────────┘     └──────────────────────────┘     └─────────────────────────┘
```

This enables latency reduction from ~1-5ms (HTTP) to ~1μs (channel).

## Usage

The `InProcessEngineClient` requires a `ConsensusEngineHandle` and `PayloadStore`,
which can be extracted from a fully-launched reth node.

### Extracting from `FullNode`

When using `NodeBuilder::with_add_ons`, you can access the engine handle from
the `AddOnsContext`:

```rust,ignore
use base_engine_ext::{InProcessEngineClient, PayloadStore};

// In a node add-ons hook or started hook:
let engine_handle = full_node.add_ons_handle.beacon_engine_handle.clone();
let payload_store = PayloadStore::new(full_node.payload_builder_handle.clone());

let client = InProcessEngineClient::new(engine_handle, payload_store);
```

### Using with `node_started_hook`

For unified binaries, use the `add_node_started_hook` to extract components
after the node is fully launched:

```rust,ignore
use base_engine_ext::{InProcessEngineClient, PayloadStore};
use tokio::sync::oneshot;

let (tx, rx) = oneshot::channel();

let builder = node_builder.add_node_started_hook(move |full_node| {
    let engine_handle = full_node.add_ons_handle.beacon_engine_handle.clone();
    let payload_store = PayloadStore::new(full_node.payload_builder_handle.clone());
    let client = InProcessEngineClient::new(engine_handle, payload_store);
    let _ = tx.send(client);
});

// Later, receive the client:
let client = rx.await?;
```

### Engine API Methods

Once constructed, the client provides direct access to Engine API methods:

```rust,ignore
// Send a new payload (no RPC serialization overhead)
let status = client.new_payload_v3(payload, versioned_hashes, parent_root).await?;

// Update fork choice state
let response = client.fork_choice_updated_v3(state, Some(attributes)).await?;

// Retrieve a built payload
let envelope = client.get_payload_v3(payload_id).await?;
```

## Supported Methods

| Method | Description |
|--------|-------------|
| `new_payload_v2` | Engine API v2 new payload |
| `new_payload_v3` | Engine API v3 new payload (Cancun) |
| `new_payload_v4` | Engine API v4 new payload (Isthmus) |
| `fork_choice_updated_v2` | Engine API v2 fork choice update |
| `fork_choice_updated_v3` | Engine API v3 fork choice update |
| `get_payload_v2` | Retrieve payload by ID (v2 envelope) |
| `get_payload_v3` | Retrieve payload by ID (v3 envelope) |
| `get_payload_v4` | Retrieve payload by ID (v4 envelope) |
