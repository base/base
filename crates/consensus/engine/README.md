# `base-consensus-engine`

<a href="https://crates.io/crates/base-consensus-engine"><img src="https://img.shields.io/crates/v/base-consensus-engine.svg?label=base-consensus-engine&labelColor=2a2f35" alt="base-consensus-engine"></a>

An extensible implementation of the [Base][base-specs] rollup node engine client.

## Overview

The `base-consensus-engine` crate provides an engine client for interacting with Ethereum execution layers. It implements the Engine API specification and manages execution layer state through direct engine methods and a small priority queue for remaining derived-state operations.

## Key Components

- **[`Engine`](crate::Engine)** - Main engine state owner that executes engine operations atomically
- **[`EngineClient`](crate::EngineClient)** - HTTP client for Engine API communication with JWT authentication
- **[`EngineState`](crate::EngineState)** - Tracks the current state of the execution layer
- **Task Types** - Specialized tasks for remaining queued engine operations:
  - [`ConsolidateTask`](crate::ConsolidateTask) - Consolidate unsafe payloads to advance the safe chain
  - [`FinalizeTask`](crate::FinalizeTask) - Finalize safe payloads on L1 confirmation
  - [`SynchronizeTask`](crate::SynchronizeTask) - Internal task for execution layer forkchoice synchronization

## Architecture

The engine owns state directly. Sequencer build, get-payload, and insert operations call `Engine` methods directly, while remaining derived-state operations are still queued and executed atomically:

```text
┌─────────────┐    ┌──────────────┐    ┌─────────────┐
│   Engine    │◄───┤  Task Queue  │◄───┤  Engine     │
│   Client    │    │   (Priority) │    │  Tasks      │
└─────────────┘    └──────────────┘    └─────────────┘
       │                   │                   │
       ▼                   ▼                   ▼
┌─────────────┐    ┌──────────────┐    ┌─────────────┐
│ Engine API  │    │ Engine State │    │  Rollup     │
│ (HTTP/JWT)  │    │   Updates    │    │  Config     │
└─────────────┘    └──────────────┘    └─────────────┘
```

- **Automatic Forkchoice Handling**: [`Engine::build`](crate::Engine::build) automatically performs forkchoice updates during block building, eliminating the need for explicit forkchoice management in user code.
- **Internal Synchronization**: [`SynchronizeTask`](crate::SynchronizeTask) handles internal execution layer synchronization and is primarily used by direct engine methods and other tasks rather than directly by users.
- **Priority-Based Execution**: Tasks are executed in priority order to ensure optimal sequencer performance and block processing efficiency.

## Engine API Compatibility

The crate supports multiple Engine API versions with automatic version selection based on the rollup configuration:

- **Engine Forkchoice Updated**: V2, V3
- **Engine New Payload**: V2, V3, V4
- **Engine Get Payload**: V2, V3, V4

Version selection follows Base hardfork activation times (Bedrock, Canyon, Delta, Ecotone, Isthmus).

## Features

- `metrics` - Enable Prometheus metrics collection (optional)

## Module Organization

- **Task Queue** - Remaining queued execution logic via [`Engine`](crate::Engine)
- **Client** - HTTP client for Engine API communication via [`EngineClient`](crate::EngineClient)
- **State** - Engine state management and synchronization via [`EngineState`](crate::EngineState)
- **Versions** - Engine API version selection via [`EngineForkchoiceVersion`](crate::EngineForkchoiceVersion),
  [`EngineNewPayloadVersion`](crate::EngineNewPayloadVersion), [`EngineGetPayloadVersion`](crate::EngineGetPayloadVersion)
- **Attributes** - Payload attribute validation via [`AttributesMatch`](crate::AttributesMatch)
- **Kinds** - Engine client type identification via [`EngineKind`](crate::EngineKind)
- **Query** - Engine query interface via [`EngineQueries`](crate::EngineQueries)
- **Metrics** - Optional Prometheus metrics collection via [`Metrics`](crate::Metrics)

<!-- Hyper Links -->

[base-specs]: https://specs.base.org

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-engine = { workspace = true }
```

Call engine operations through `Engine`:

```rust,ignore
use base_consensus_engine::{Engine, EngineClient};

let client = EngineClient::new(engine_url, jwt_secret)?;
let mut engine = Engine::new(initial_state, state_tx, queue_tx);

engine.insert_unsafe_payload(client, rollup_config, payload).await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
