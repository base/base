# `base-consensus-node`

<a href="https://crates.io/crates/base-consensus-node"><img src="https://img.shields.io/crates/v/base-consensus-node.svg" alt="base-consensus-node crate"></a>
<a href="https://specs.base.org"><img src="https://img.shields.io/badge/Docs-854a15?style=flat&labelColor=1C2C2E&color=BEC5C9&logo=mdBook&logoColor=BEC5C9" alt="Docs" /></a>

An implementation of the Base [RollupNode][rn-spec] service.

[rn-spec]: https://specs.base.org/protocol/consensus

## Overview

This crate wires together every subsystem of the Base consensus node into a single runnable service. It owns no domain logic itself — derivation lives in `base-consensus-derive`, engine state management lives in `base-consensus-engine`, peer-to-peer gossip lives in `base-consensus-gossip`, and so on. What this crate provides is the composition layer: it constructs each subsystem as an independent async actor, opens the typed channels between them, and manages the shared lifetime of all actors through a single `CancellationToken`.

The entry point for most callers is `RollupNodeBuilder`, which accepts the required configuration for each subsystem and produces a `RollupNode` whose `start()` method blocks until the process receives SIGINT or SIGTERM or until any actor exits with an error.

## Actor Model

The service is built around a strict actor model. Each actor is a struct that implements the `NodeActor` trait, which has a single async method `start(self, context: Self::StartData) -> Result<(), Self::Error>`. Actors do not share mutable state. Instead, all cross-actor communication happens through typed channels: `tokio::sync::mpsc` channels for requests and `tokio::sync::watch` channels for broadcast state. Every actor holds a `CancellationToken` cloned from the root token created at startup; when any actor exits with an error the token is cancelled, which causes all remaining actors to observe their cancellation futures and exit cleanly.

The `spawn_and_wait!` macro in `service/util.rs` is the mechanism that brings all actors up together. It spawns each actor onto a `tokio::task::JoinSet`, then drives a `select!` loop that listens simultaneously for OS shutdown signals and for the first task to complete. On either event it cancels the token and then waits for remaining tasks to drain. Errors from individual tasks are formatted and returned as `Err(String)` from the top-level `start()` method.

## Service Variants

The crate exposes two top-level service types: `RollupNode` and `FollowNode`.

`RollupNode` is the full consensus node. It runs the complete L1 derivation pipeline, participates in the P2P network, and optionally runs the sequencer. It is constructed through `RollupNodeBuilder`, which requires a `RollupConfig`, an `L1ConfigBuilder`, a boolean for whether to trust the L2 RPC, an `EngineConfig` carrying the L2 engine RPC URL and JWT secret, and a `NetworkConfig`. Optional extensions include an RPC server (`RpcBuilder`), sequencer behavior (`SequencerConfig`), a derivation delegation endpoint (`DerivationDelegateConfig`), a persistent safe-head database path, and a finalized block poll interval override.

`FollowNode` is a stripped-down variant that does not run derivation at all. Instead it polls a remote L2 execution layer node via RPC and drives the local engine to mirror that node's chain. It spawns only the engine actor, the delegate-L2 derivation actor, the L1 watcher actor, and optionally the RPC actor. It is used in contexts where a node needs to track the canonical chain without performing the full derivation computation.

## `RollupNode` Mode

`RollupNode` is mode-aware. The mode is stored in `EngineConfig::mode` as the `NodeMode` enum with two variants: `Validator` and `Sequencer`. In `Validator` mode the engine bootstraps by seeding state from the execution layer's existing head, signals the derivation pipeline, and waits for derivation to drive safe-head updates. In `Sequencer` mode the engine bootstraps differently — either with a conductor-follower probe using zeroed safe/finalized heads, or as an active sequencer that probes with real heads or resets at genesis — and additionally publishes an `unsafe_head_tx` watch channel that the sequencer actor reads to know the current building parent.

## Construction and Startup Sequence

When `RollupNodeBuilder::build()` is called, it constructs the `L1Config` (containing a `OnlineBeaconClient`, an Alloy `RootProvider` for L1, and consensus parameters), connects to the L2 execution layer via `BaseEngineClient`, and optionally builds a `DerivationDelegateClient` if delegation is configured. The result is a `RollupNode` with all configuration resolved but no actors running yet.

Calling `RollupNode::start()` begins the actor wiring phase. The sequence is deterministic:

First, a root `CancellationToken` is created. If a safe-head database path was provided, a `SafeDB` backed by redb is opened; otherwise a `DisabledSafeDB` is used. In delegation mode the database is always disabled.

Second, the core channels are created. There is one `mpsc` channel of capacity 1024 for `DerivationActorRequest`, one of capacity 1024 for `EngineActorRequest`, and one `watch` channel for the unsafe L2 head (`L2BlockInfo`).

Third, the engine actor is constructed. It wraps a `BaseEngineClient` together with an `Engine` task queue, an `EngineProcessor` that holds the bootstrap logic and main processing loop, and an `EngineRpcProcessor` that handles concurrent RPC queries behind a semaphore of size 16. The engine actor receives on its `mpsc::Receiver<EngineActorRequest>` and routes each request either to the processing task or to the RPC task based on the request variant.

Fourth, the derivation actor is created. In normal mode this is a `DerivationActor` wrapping an `OnlinePipeline` from `base-consensus-derive`. In delegation mode this is a `DelegateDerivationActor` that polls an external `optimism_syncStatus` RPC endpoint instead of running the pipeline. Both communicate to the engine through a `QueuedDerivationEngineClient` that wraps `mpsc::Sender<EngineActorRequest>`.

Fifth, the network actor is created via `NetworkActor::new()`. This async constructor builds the libp2p gossipsub swarm and the discv5 discovery daemon, starts both, optionally updates the local ENR with the discovered external address, and returns a `NetworkInboundData` struct containing the sender halves of four channels: the `signer` channel (capacity 16) for updating the unsafe block signer address, the `p2p_rpc` channel (capacity 1024) for P2P RPC requests, the `admin_rpc` channel (capacity 1024) for network admin queries, and the `gossip_payload_tx` channel (capacity 256) for payloads the sequencer wants to gossip.

Sixth, the L1 watcher actor is created. It receives two `BlockStream` instances — one polling `eth_getBlockByNumber("latest")` at a 4-second interval and one polling `eth_getBlockByNumber("finalized")` at a configurable interval — and an `Arc<AtomicU64>` shared with the `ConfDepthProvider` so that the derivation pipeline knows the L1 head without polling again. When a new L1 head arrives, the watcher applies the `verifier_l1_confs` offset, fetches the delayed block by number, and forwards it to the derivation actor. It also fetches `SystemConfigLog` events from the L1 system config address on each head update, extracting any `UnsafeBlockSigner` update and forwarding the new signer address to the network actor's `signer` channel. A separate `L1WatcherQueryProcessor` runs concurrently (up to 32 concurrent queries) and handles point-in-time `L1WatcherQueries` from the RPC layer.

Seventh, if the node is in sequencer mode, the sequencer actor is created. It receives the `watch::Receiver<L2BlockInfo>` for the unsafe head, a `QueuedUnsafePayloadGossipClient` wrapping `gossip_payload_tx`, a `PayloadBuilder` carrying the `L1OriginSelector` and `StatefulAttributesBuilder`, an optional `ConductorClient`, and a `RecoveryModeGuard` (an `Arc<AtomicBool>` shared between the actor and the builder). An `mpsc` channel of capacity 1024 is created for `SequencerAdminQuery` messages; the sender half is returned as a `QueuedSequencerAdminAPIClient` for use by the RPC actor.

Eighth, if an `RpcBuilder` was provided, the RPC actor is created. It wraps a `QueuedEngineRpcClient`, an optional `QueuedSequencerAdminAPIClient`, and the `Arc<dyn SafeDBReader>`. The `RpcContext` passed at startup carries the sender halves of the p2p RPC, network admin, and L1 watcher query channels.

Finally, `spawn_and_wait!` spawns all constructed actors onto the `JoinSet` and enters the shutdown monitoring loop.

## Engine Actor

The engine actor is the hub of the service. All other actors that need to affect the L2 execution layer send requests to it. It owns the `Engine` struct from `base-consensus-engine`, which maintains the task queue and the `EngineState` watch channel. The actor's main loop receives `EngineActorRequest` variants and routes them:

`BuildRequest` carries a `PayloadId` response channel and is forwarded to the processing task, which calls `engine_api::forkchoice_updated` with the payload attributes to begin block building and returns the resulting `PayloadId`.

`GetPayloadRequest` carries attributes and a result channel; the processing task calls `engine_api::get_payload` and returns the sealed `BaseExecutionPayloadEnvelope`.

`SealRequest` is similar to `GetPayloadRequest` but used in the sequencer path where the result will be gossiped and then inserted.

`ProcessUnsafeL2BlockRequest` is fire-and-forget. It takes a `BaseExecutionPayloadEnvelope` received from the P2P network or from the sequencer's insert step, calls `engine_api::new_payload`, then calls `engine_api::forkchoice_updated` to make the block the new unsafe head.

`ProcessSafeL2SignalRequest` takes a `ConsolidateInput` from the derivation actor — either a derived set of `AttributesWithParent` or a delegated `L2BlockInfo` — and runs the safe-head consolidation path: attributes are forwarded to `engine_api::forkchoice_updated`, and the resulting safe head is sent back to the derivation actor via the `QueuedEngineDerivationClient`.

`ResetRequest` triggers a full engine reset: the processing task calls the reset procedure on the `Engine`, clears in-flight state, and sends a `ResetSignal` to the derivation actor so the pipeline rewinds to the last safe head.

`RpcRequest` is routed to the RPC processor task. It handles `EngineQueries` variants — `Config`, `State`, `OutputAtBlock`, `TaskQueueLength`, `QueueLengthReceiver`, `StateReceiver` — all of which read from the engine's watch channels or perform point-in-time queries without affecting processing state.

At startup, the engine processor determines its bootstrap role. If the node is a validator it calls `bootstrap_validator()`, which seeds `unsafe_head`, `safe_head`, and `finalized_head` from the execution layer's current state without sending any forkchoice update, leaving `el_sync_complete` as false until the derivation actor signals completion. If the node is an active sequencer it calls `bootstrap_active_sequencer()`, which resets the engine at genesis or probes it with real safe/finalized heads to establish the initial forkchoice state. If the conductor-follower role is resolved it calls `bootstrap_conductor_follower()`, which probes with zeroed safe/finalized heads.

After bootstrap, the processing loop drains the engine task queue on every iteration before waiting for the next request. Drain errors are classified by severity: `Critical` errors halt the actor, `Reset` errors trigger a pipeline reset, `Flush` errors flush invalid payloads from the queue, and `Temporary` errors are logged and retried.

## Derivation Actor

The derivation actor drives the `OnlinePipeline` from `base-consensus-derive` and translates its output into engine requests. It maintains a `DerivationStateMachine` with six states.

The initial state is `AwaitingELSyncCompletion`. The actor waits in this state until the engine sends a `ProcessEngineSyncCompletionRequest` via `QueuedEngineDerivationClient::notify_sync_completed()`, at which point the state transitions to `Deriving`.

In `Deriving` the actor calls `pipeline.step()` in a loop. Each call either returns `PreparedAttributes` — a set of `AttributesWithParent` ready to send to the engine — or returns an error. On `PreparedAttributes` the actor transitions to `AwaitingSafeHeadConfirmation`, enqueues the attributes in the `L2Finalizer`, records the L1 inclusion block as `pending_derived_from`, and sends a `ProcessSafeL2SignalRequest` to the engine. On `NotEnoughData` it yields and transitions to `AwaitingL1Data`. On a reset error (reorg detected or Holocene activation) it sends a `ProcessEngineSignalRequest` and transitions to `AwaitingSignal`.

The `AwaitingSafeHeadConfirmation` state persists until the engine actor confirms the attributes by calling back through `QueuedEngineDerivationClient::send_new_engine_safe_head()`. That call generates a `ProcessEngineSafeHeadUpdateRequest`, which records the new safe head in the `SafeDB` (paired with the L1 block from `pending_derived_from`) and transitions back to `Deriving` to produce the next batch of attributes.

The `L2Finalizer` struct tracks finalization by maintaining a `BTreeMap<u64, u64>` from L1 block number to the highest L2 block number derived in that epoch. When the L1 watcher reports a new finalized L1 block, the actor calls `L2Finalizer::try_finalize_next()`, which scans the map for all entries at or below the finalized L1 number, drains them, and returns the highest L2 block number. That number is forwarded to the engine as a `ProcessFinalizedL2BlockNumberRequest`.

The delegation variants differ significantly. `DelegateDerivationActor` polls an external `optimism_syncStatus` endpoint every 4 seconds, validates the reported safe and finalized L2 blocks' L1 origins against its local L1 provider for hash consistency, and then forwards the safe and finalized L2 heads to the engine. It does not run a pipeline at all. `DelegateL2DerivationActor` is used by `FollowNode`; it polls the source L2 node's head by block number every 2 seconds, fetches each missing payload, sends them to the engine as `ProcessUnsafeL2BlockRequest` one at a time, and then issues a delegated forkchoice update after the batch is complete.

## Sequencer Actor

The sequencer actor owns the block production loop. It runs a `tokio::select!` with five arms in priority order: cancellation, admin queries, the seal pipeline step, the build ticker (gated on `sealer.is_none()`), and initial reset retries before the main loop starts.

The ticker fires every `rollup_config.block_time` seconds (default two seconds) and is wall-clock synchronized. After each successful seal the next tick is scheduled for `UNIX_EPOCH + (sealed_block_timestamp + block_time) * 1s - last_seal_duration`, so that the ticker fires slightly early to account for the time taken by the seal pipeline.

The build-seal cycle works as follows. On each tick, if there is a payload handle from a previous `start_build_block` call, the sequencer calls `get_sealed_payload` to retrieve the finalized `BaseExecutionPayloadEnvelope` from the engine. If the build is stale — the sealed envelope's parent does not match `next_build_parent` — it is discarded and a fresh build is started. If the seal succeeds, `next_build_parent` is computed from the sealed envelope's header so the next build will be anchored to the correct parent, and the `PayloadSealer` is constructed to drive the three-stage pipeline.

The `PayloadSealer` runs one async operation per call to `step()`. In the first stage (`Sealed`) it calls `conductor.commit_unsafe_payload()` if a conductor is configured, or skips to the next stage if not. In the second stage (`Committed`) it calls `gossip_client.schedule_execution_payload_gossip()`, which is a fire-and-forget send to the network actor's `gossip_payload_tx` channel. In the third stage (`Gossiped`) it calls `engine_client.insert_unsafe_payload()`, another fire-and-forget that sends `ProcessUnsafeL2BlockRequest` to the engine actor. On `Ok(true)` from `step()` the sealer is dropped and the build ticker resumes. If any step returns an error the sequencer logs a warning and retries on the next select iteration.

Parallel to the seal pipeline, the `PayloadBuilder` drives the next block's preparation. `build()` reads the current unsafe head from the watch channel via `engine_client.get_unsafe_head()` and delegates to `build_on(parent)`. Inside `build_on`, the `L1OriginSelector` determines the next L1 epoch: it consults its cached current and next L1 origins, advances to the next epoch if the L2 timestamp has caught up, and respects `max_sequencer_drift` to decide when empty blocks must be produced because the next L1 block is unavailable. The origin selector is wrapped in `DelayedL1OriginSelectorProvider`, which gates block-number lookups to only return blocks at or below `l1_head - l1_conf_delay`. After origin selection, `StatefulAttributesBuilder::prepare_payload_attributes()` from `base-consensus-derive` produces the `OpPayloadAttributes`, the `PoolActivation` check determines whether to include transactions from the mempool or produce an empty block (recovery mode, sequencer drift, or hardfork activation blocks all force empty blocks), and `start_build_block()` sends a `BuildRequest` to the engine actor and returns the `PayloadId`.

The `RecoveryModeGuard` is an `Arc<AtomicBool>` shared between the sequencer actor and the payload builder. Setting recovery mode via the admin API writes to the atomic, and the builder reads it on every build attempt. When recovery mode is true, `PoolActivation::is_enabled()` returns false and empty blocks are produced.

## Network Actor

The network actor manages the libp2p gossipsub swarm and the discv5 discovery daemon. It exposes four inbound channels to the rest of the service. Other actors send payloads to `gossip_payload_tx` (capacity 256) when they want blocks propagated, send address updates to `signer` (capacity 16) when the unsafe block signer changes, send P2P RPC queries to `p2p_rpc` (capacity 1024), and send admin queries to `admin_rpc` (capacity 1024).

The `GossipTransport` trait abstracts the transport backend. The production implementation is `NetworkHandler`, which wraps a `GossipDriver<ConnectionGater>` (the libp2p swarm), a `Discv5Handler`, an `mpsc::Receiver<Enr>` from discovery, a `watch::Sender<Address>` for the signer broadcast, and optionally a `BlockSignerHandler`. The `NetworkHandler::publish()` method signs the payload, constructs a `NetworkPayloadEnvelope`, and publishes it to the correct gossipsub topic based on the payload timestamp relative to hardfork activation. `NetworkHandler::next_unsafe_block()` runs a `select!` that drains gossipsub events through `GossipDriver::handle_event()`, dials newly discovered peers from the ENR receiver, and periodically inspects peer scores to disconnect and ban peers below the configured ban threshold.

The `NetworkDriver::build()` produces the `NetworkHandler` by starting the libp2p swarm, resolving the local external address from the TCP listen multiaddr, updating the discv5 ENR socket, starting the discv5 daemon, and wiring together the peer score interval.

## L1 Watcher Actor

The L1 watcher actor is the service's source of truth for L1 chain state. It runs two concurrent streams: `head_stream` polls `eth_getBlockByNumber("latest")` every four seconds and `finalized_stream` polls `eth_getBlockByNumber("finalized")` at the interval configured in `L1Config`. Both streams are deduplicated — they only emit when the block changes.

On each new head, the watcher computes the confirmation-delayed block number as `head.number - verifier_l1_confs`. If the delayed number is reachable it fetches that block by number via `AlloyL1BlockFetcher::get_block()` and sends it to the derivation actor as a `ProcessL1HeadUpdateRequest`. It also broadcasts the real head through the `watch::Sender<Option<BlockInfo>>` and stores the real head number in the shared `Arc<AtomicU64>` so the `ConfDepthProvider` used by the derivation pipeline can gate its own L1 lookups.

Log fetching runs on the same head-update path. The watcher calls `AlloyL1BlockFetcher::get_logs()` with a filter for `SystemConfigLog` events from the rollup config's L1 system config address. If the logs contain a `SystemConfigUpdate::UnsafeBlockSigner` event it extracts the new signer address and sends it to the network actor via the `block_signer_sender` channel. The log fetch retries up to ten times with exponential backoff from 50 ms to 500 ms before the actor returns an error.

On each finalized block, the watcher sends the block directly to the derivation actor as a `ProcessFinalizedL1Block` without any confirmation delay.

The `L1WatcherQueryProcessor` runs as a separate actor that receives `L1WatcherQueries` from the RPC layer over an `mpsc::Receiver<L1WatcherQueries>` (capacity 1024) and processes them with concurrency up to 32 via `for_each_concurrent`. The two supported query types are `Config`, which returns the `RollupConfig`, and `L1State`, which assembles a point-in-time `L1State` struct by reading the watch channel and issuing `eth_blockNumber` calls for latest, safe, and finalized tags.

## RPC Actor

The RPC actor wraps a `jsonrpsee` HTTP server and optionally a WebSocket server. It builds the server module set based on what was configured: `HealthzRpc` is always present, `P2pRpc` is included when a p2p channel sender is available, `AdminRpc` is included when a network admin channel sender is available, `RollupRpc` carries the engine RPC client and L1 watcher query client together with the `SafeDBReader`, `DevEngineRpc` is included in dev mode, and `WsRPC` is included when WebSocket is enabled.

`QueuedEngineRpcClient` implements the `EngineRpcClient` trait by sending `EngineActorRequest::RpcRequest` messages and awaiting oneshot responses. All queries go through the engine actor's main channel and are dispatched to the `EngineRpcProcessor`, which handles them concurrently behind the semaphore.

`QueuedSequencerAdminAPIClient` implements the `SequencerAdminAPIClient` trait by sending `SequencerAdminQuery` messages with oneshot response channels. The sequencer actor services these in the high-priority admin arm of its `select!` loop.

The RPC actor monitors the server handle for unexpected stops. If the server stops before the cancellation token fires it can restart up to a configured number of times. After exhausting restarts it cancels the root token, which brings down the entire service.

## Channel Wiring Summary

The complete channel graph between actors is as follows. The L1 watcher actor writes new L1 heads and finalized L1 blocks to the derivation actor via `mpsc::Sender<DerivationActorRequest>` (capacity 1024). The L1 watcher actor also broadcasts the raw L1 head through `watch::Sender<Option<BlockInfo>>` to any subscribers (currently consumed by the sequencer's `ConfDepthProvider` indirectly via the shared atomic). The derivation actor writes `ConsolidateInput` signals, finalized L2 block numbers, and delegated forkchoice updates to the engine actor via `mpsc::Sender<EngineActorRequest>` (capacity 1024). The engine actor notifies the derivation actor of safe-head updates and sync completion via `mpsc::Sender<DerivationActorRequest>` on the same channel (but from the opposite direction). The sequencer actor sends build requests, seal requests, get-payload requests, insert-unsafe requests, and reset requests to the engine actor via the same `mpsc::Sender<EngineActorRequest>`. The engine actor publishes the current unsafe head to the sequencer via `watch::Sender<L2BlockInfo>`. The network actor forwards inbound gossip payloads to the engine actor via `mpsc::Sender<EngineActorRequest>` through `QueuedNetworkEngineClient`. The sequencer actor sends payloads to gossip via `mpsc::Sender<BaseExecutionPayloadEnvelope>` (capacity 256) to the network actor's `publish_rx`. The L1 watcher actor sends signer address updates via `mpsc::Sender<Address>` (capacity 16) to the network actor's `signer` receiver. The RPC actor sends engine queries via `mpsc::Sender<EngineActorRequest>` and sequencer admin queries via `mpsc::Sender<SequencerAdminQuery>` (capacity 1024). The RPC actor sends P2P RPC requests via `mpsc::Sender<P2pRpcRequest>` (capacity 1024) and network admin queries via `mpsc::Sender<NetworkAdminQuery>` (capacity 1024) to the network actor. The RPC actor sends `L1WatcherQueries` via `mpsc::Sender<L1WatcherQueries>` (capacity 1024) to the `L1WatcherQueryProcessor`.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
