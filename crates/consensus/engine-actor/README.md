# base-engine-actor

Engine actor for direct reth communication via in-process channels.

## Overview

This crate provides `DirectEngineActor`, which implements kona's `NodeActor` trait
to integrate with kona-node-service. It uses `InProcessEngineClient` from
`base-engine-ext` to communicate directly with reth's consensus engine.

## Architecture

```text
┌─────────────────────────────────────────────────────────────────────────────────┐
│                           kona-node-service                                     │
│                                                                                 │
│  ┌───────────────────┐  ┌───────────────────┐  ┌────────────────────────────┐  │
│  │  DerivationActor  │  │    SyncActor      │  │     DirectEngineActor      │  │
│  └─────────┬─────────┘  └─────────┬─────────┘  └──────────────┬─────────────┘  │
│            │                      │                           │                │
│            └──────────────────────┼───────────────────────────┘                │
│                                   │                                            │
│                          EngineActorRequest                                    │
│                                   │                                            │
└───────────────────────────────────┼────────────────────────────────────────────┘
                                    │
                                    ▼
                    ┌───────────────────────────────┐
                    │   DirectEngineProcessor       │
                    │                               │
                    │  ┌─────────────────────────┐  │
                    │  │ BuildHandler            │  │
                    │  │ SealHandler             │  │
                    │  │ ConsolidationHandler    │  │
                    │  │ FinalizationHandler     │  │
                    │  │ UnsafeBlockHandler      │  │
                    │  │ ResetHandler            │  │
                    │  └─────────────────────────┘  │
                    └───────────────┬───────────────┘
                                    │
                                    ▼
                    ┌───────────────────────────────┐
                    │   InProcessEngineClient<P>    │
                    │   (from base-engine-ext)      │
                    └───────────────┬───────────────┘
                                    │
                                    │ unbounded channel
                                    ▼
                    ┌───────────────────────────────┐
                    │   reth EngineApiTreeHandler   │
                    └───────────────────────────────┘
```

## Request Types

The actor handles several request types from kona-node-service:

| Request | Description |
|---------|-------------|
| `Build` | Initiates payload building via FCU with attributes |
| `Seal` | Gets and inserts the built payload |
| `ProcessSafeL2Signal` | Updates the safe head |
| `ProcessFinalizedL2BlockNumber` | Updates the finalized head |
| `ProcessUnsafeL2Block` | Inserts an unsafe block received from gossip |
| `Reset` | Resets to the finalized head |

## Usage

```rust,ignore
use base_engine_actor::DirectEngineActor;
use base_engine_ext::InProcessEngineClient;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// Create the engine client
let client = InProcessEngineClient::new(engine_handle, payload_store, provider);

// Create request channel
let (tx, rx) = mpsc::channel(256);

// Create the actor
let actor = DirectEngineActor::new(client, rx, CancellationToken::new());

// Start the actor (implements NodeActor)
actor.start(rollup_config).await?;
```
