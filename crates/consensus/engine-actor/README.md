# base-engine-actor

Custom engine actor implementation for direct in-process communication with reth,
bypassing the HTTP Engine API.

## Why This Exists

Kona's engine actor is tightly coupled to HTTP transport. We cannot use it directly
because of several architectural constraints in the upstream codebase.

### 1. EngineClient Trait Hardcodes HTTP

The [`EngineClient`](https://github.com/op-rs/kona/blob/24e7e2658e09ac00c8e6cbb48bebe6d10f8fb69d/crates/node/engine/src/client.rs#L67) trait extends `OpEngineApi` with HTTP-specific bounds:

```rust,ignore
pub trait EngineClient: OpEngineApi<Optimism, Http<HyperAuthClient>> + Send + Sync { ... }
```

This means any implementation must use HTTP transport - there's no way to inject
an in-process implementation that bypasses network serialization.

### 2. RollupNode Fields Are Private

[`RollupNode`](https://github.com/op-rs/kona/blob/24e7e2658e09ac00c8e6cbb48bebe6d10f8fb69d/crates/node/service/src/service/node.rs#L49-L70) uses `pub(crate)` for all configuration fields:

```rust,ignore
pub struct RollupNode {
    pub(crate) config: Arc<RollupConfig>,
    pub(crate) engine_config: EngineConfig,
    // ... all fields pub(crate)
}
```

We cannot access these from outside the crate to build custom actors with alternative
configurations.

### 3. Engine Actor Creation Is Private

The [`create_engine_actor()`](https://github.com/op-rs/kona/blob/24e7e2658e09ac00c8e6cbb48bebe6d10f8fb69d/crates/node/service/src/service/node.rs#L185-L233) method is private (`fn`, not `pub fn`) and hardcodes `OpEngineClient`:

```rust,ignore
fn create_engine_actor(...) -> Result<
    EngineActor<
        EngineProcessor<OpEngineClient<RootProvider, RootProvider<Optimism>>, ...>,
        EngineRpcProcessor<OpEngineClient<...>>,
    >,
    String,
>
```

This prevents external code from substituting a custom engine client implementation.

### 4. EngineActor Is Tightly Coupled to HTTP Client

The `EngineActor` in kona creates and uses an HTTP-bound engine client internally.
All engine operations (forkchoice updates, payload building) go through this HTTP client,
making it impossible to inject an in-process driver without reimplementing the actor.

## What We Reuse From Kona

We import these public types/traits from kona without modification:

- `EngineActorRequest` - Message enum for inter-actor communication
- `EngineContext` - Context passed to engine actor on start
- `EngineDerivationClient` - Communication with derivation actor
- `NodeActor` - Actor trait for unified spawning
- `NetworkActor`, `DerivationActor`, `L1WatcherActor` - Used unchanged in our orchestrator

See the [kona-node-service actors](https://github.com/op-rs/kona/tree/main/crates/node/service/src/actors) for the source.

## What We Reimplement

We port these components, making them generic over [`DirectEngineApi`](../../shared/engine-driver/) instead of kona's HTTP-bound `EngineClient`:

| Component | Kona Equivalent | Our Implementation |
|-----------|-----------------|-------------------|
| `EngineActor` | `EngineActor` | `DirectEngineActor` |
| Engine request processing | `EngineActor::start()` loop | `DirectEngineProcessor` |
| Engine request handlers | Built into EngineActor | `handlers/*` modules |

## Architecture

```text
┌────────────────────────────────────────────────────────────────────────┐
│  UnifiedRollupNode (orchestrator)                                       │
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
│  │ DirectEngineActor (this crate)                               │       │
│  │   └─ DirectEngineProcessor<InProcessEngineDriver>            │       │
│  └───────────────────┬─────────────────────────────────────────┘       │
│                      │                                                  │
│                      ▼ (in-process channel via ConsensusEngineHandle)  │
│  ┌─────────────────────────────────────────────────────────────┐       │
│  │ reth EL (EngineApiTreeHandler)                               │       │
│  └─────────────────────────────────────────────────────────────┘       │
└────────────────────────────────────────────────────────────────────────┘
```

## Module Structure

- `actor.rs` - `DirectEngineActor` wrapper that implements `NodeActor`
- `processor.rs` - `DirectEngineProcessor` main request processing loop
- `request.rs` - `EngineProcessingRequest` and `RoutedRequest` types
- `error.rs` - `DirectEngineProcessorError` error type
- `handlers/` - Individual request handlers:
  - `build.rs` - Block building requests
  - `seal.rs` - Block sealing requests
  - `consolidation.rs` - Derived L2 attributes processing
  - `finalization.rs` - L1 finalization processing
  - `unsafe_block.rs` - Unsafe block processing from P2P
  - `reset.rs` - Engine reset handling

## Benefits

- **Zero serialization overhead**: No JSON-RPC encoding/decoding
- **No network latency**: Direct channel sends instead of HTTP/IPC round-trips
- **No JWT authentication overhead**: In-process communication is inherently trusted
- **Single process**: Simplified deployment and monitoring
- **Shared memory**: Both CL and EL can share read-only state efficiently
