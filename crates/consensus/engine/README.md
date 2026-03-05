# `base-consensus-engine`

<a href="https://crates.io/crates/base-consensus-engine"><img src="https://img.shields.io/crates/v/base-consensus-engine.svg?label=base-consensus-engine&labelColor=2a2f35" alt="base-consensus-engine"></a>

An extensible implementation of the [OP Stack][op-stack] rollup node engine client.

## Overview

The `base-consensus-engine` crate provides a task-based engine client for interacting with Ethereum execution layers. It implements the Engine API specification and manages the execution layer state through a priority-driven task queue system.

## Key Components

- **[`Engine`](crate::Engine)** - Main task queue processor that executes engine operations atomically
- **[`EngineClient`](crate::EngineClient)** - HTTP client for Engine API communication with JWT authentication
- **[`EngineState`](crate::EngineState)** - Tracks the current state of the execution layer
- **Task Types** - Specialized tasks for different engine operations:
  - [`InsertTask`](crate::InsertTask) - Insert new payloads into the execution engine
  - [`BuildTask`](crate::BuildTask) - Build new payloads with automatic forkchoice synchronization
  - [`ConsolidateTask`](crate::ConsolidateTask) - Consolidate unsafe payloads to advance the safe chain
  - [`FinalizeTask`](crate::FinalizeTask) - Finalize safe payloads on L1 confirmation
  - [`SynchronizeTask`](crate::SynchronizeTask) - Internal task for execution layer forkchoice synchronization

## Architecture

The engine implements a task-driven architecture where forkchoice synchronization is handled automatically:

- **Automatic Forkchoice Handling**: The [`BuildTask`](crate::BuildTask) automatically performs forkchoice updates during block building, eliminating the need for explicit forkchoice management in user code.
- **Internal Synchronization**: [`SynchronizeTask`](crate::SynchronizeTask) handles internal execution layer synchronization and is primarily used by other tasks rather than directly by users.
- **Priority-Based Execution**: Tasks are executed in priority order to ensure optimal sequencer performance and block processing efficiency.

## Engine API Compatibility

The crate supports multiple Engine API versions with automatic version selection based on the rollup configuration:

- **Engine Forkchoice Updated**: V2, V3
- **Engine New Payload**: V2, V3, V4
- **Engine Get Payload**: V2, V3, V4

Version selection follows Optimism hardfork activation times (Bedrock, Canyon, Delta, Ecotone, Isthmus).

## Features

- `metrics` - Enable Prometheus metrics collection (optional)

## Architecture

The engine operates as a task-driven system where operations are queued and executed atomically:

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

## Module Organization

- **Task Queue** - Core engine task queue and execution logic via [`Engine`](crate::Engine)
- **Client** - HTTP client for Engine API communication via [`EngineClient`](crate::EngineClient)
- **State** - Engine state management and synchronization via [`EngineState`](crate::EngineState)
- **Versions** - Engine API version selection via [`EngineForkchoiceVersion`](crate::EngineForkchoiceVersion),
  [`EngineNewPayloadVersion`](crate::EngineNewPayloadVersion), [`EngineGetPayloadVersion`](crate::EngineGetPayloadVersion)
- **Attributes** - Payload attribute validation via [`AttributesMatch`](crate::AttributesMatch)
- **Kinds** - Engine client type identification via [`EngineKind`](crate::EngineKind)
- **Query** - Engine query interface via [`EngineQueries`](crate::EngineQueries)
- **Metrics** - Optional Prometheus metrics collection via [`Metrics`](crate::Metrics)

<!-- Hyper Links -->

[op-stack]: https://specs.optimism.io
