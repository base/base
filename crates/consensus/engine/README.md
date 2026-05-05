# `base-consensus-engine`

<a href="https://crates.io/crates/base-consensus-engine"><img src="https://img.shields.io/crates/v/base-consensus-engine.svg?label=base-consensus-engine&labelColor=2a2f35" alt="base-consensus-engine"></a>

An extensible implementation of the [Base][base-specs] rollup node engine client.

## Overview

The `base-consensus-engine` crate provides an engine client for interacting with Ethereum execution layers. It implements the Engine API specification and manages execution layer state through direct engine methods.

## Key Components

- **[`Engine`](crate::Engine)** - Main engine state owner that executes engine operations atomically
- **[`EngineClient`](crate::EngineClient)** - HTTP client for Engine API communication with JWT authentication
- **[`EngineState`](crate::EngineState)** - Tracks the current state of the execution layer
- **[`SynchronizeTask`](crate::SynchronizeTask)** - Internal helper for execution layer forkchoice synchronization

## Architecture

The engine owns state directly. Engine actor request paths call `Engine` methods directly, keeping state mutation serialized through one mutable owner:

```text
┌─────────────┐    ┌─────────────┐    ┌─────────────┐
│ EngineActor │───▶│   Engine    │───▶│ Engine API  │
│  Requests   │    │   State     │    │ (HTTP/JWT)  │
└─────────────┘    └─────────────┘    └─────────────┘
```

- **Automatic Forkchoice Handling**: [`Engine::build`](crate::Engine::build) automatically performs forkchoice updates during block building, eliminating the need for explicit forkchoice management in user code.
- **Internal Synchronization**: [`SynchronizeTask`](crate::SynchronizeTask) handles internal execution layer synchronization and is primarily used by direct engine methods rather than directly by users.
- **Serialized Execution**: The engine actor owns one mutable [`Engine`](crate::Engine), so each request updates execution-layer state before the next request is processed.

## Engine API Compatibility

The crate supports multiple Engine API versions with automatic version selection based on the rollup configuration:

- **Engine Forkchoice Updated**: V2, V3
- **Engine New Payload**: V2, V3, V4
- **Engine Get Payload**: V2, V3, V4

Version selection follows Base hardfork activation times (Bedrock, Canyon, Delta, Ecotone, Isthmus).

## Features

- `metrics` - Enable Prometheus metrics collection (optional)

## Module Organization

- **Engine** - State owner and operation logic via [`Engine`](crate::Engine)
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
let mut engine = Engine::new(initial_state, state_tx);

engine.insert_unsafe_payload(client, rollup_config, payload).await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
