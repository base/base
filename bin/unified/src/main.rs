//! Unified CL+EL binary for Base.
//!
//! This binary runs both the consensus layer (kona) and execution layer (reth)
//! in a single process, with direct in-process channel communication instead of
//! the Engine API over HTTP or IPC.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │                         Unified Binary                                   │
//! ├──────────────────────────────────────────────────────────────────────────┤
//! │  Phase 1: Launch reth EL via BaseNodeRunner                              │
//! │  Phase 2: Extract ConsensusEngineHandle + PayloadStore                   │
//! │  Phase 3: Create InProcessEngineDriver and start DirectEngineActor       │
//! │  Phase 4: Wire up DerivationActor, NetworkActor, L1WatcherActor          │
//! └──────────────────────────────────────────────────────────────────────────┘
//!                                     │
//!          ┌───────────────────────────┴───────────────────────────┐
//!          ▼                                                       ▼
//! ┌────────────────────────┐                         ┌────────────────────────┐
//! │  reth Execution Layer  │◄────────────────────────│  kona Consensus Layer  │
//! │  ───────────────────── │   InProcessEngineDriver │  ────────────────────  │
//! │  • ConsensusEngineHandle│   (channel-based)      │  • DirectEngineActor   │
//! │  • PayloadStore        │                         │  • DerivationActor     │
//! │  • Provider            │                         │  • NetworkActor        │
//! │                        │                         │  • L1WatcherActor      │
//! └────────────────────────┘                         └────────────────────────┘
//! ```
//!
//! # Actor Wiring
//!
//! When L1 RPC and beacon endpoints are provided (`--l1-rpc-url` and `--l1-beacon-url`),
//! the full actor stack is wired up:
//!
//! - **DirectEngineActor**: Processes forkchoice updates and payload building via
//!   direct in-process channels to reth's engine.
//! - **DerivationActor**: Derives L2 blocks from L1 data using the derivation pipeline.
//! - **L1WatcherActor**: Monitors L1 for new heads and finalized blocks.
//! - **NetworkActor**: Handles P2P gossip for unsafe blocks.

#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use std::{sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use base_client_node::{BaseNodeRunner, EngineComponents, UnifiedConfig, UnifiedExtension};
use base_engine_bridge::InProcessEngineDriver;
use base_node_service::{NodeActor, RollupConfig, UnifiedRollupNode};
use clap::Parser;
use eyre::Result;
use futures::StreamExt;
use kona_node_service::{
    DerivationActor, DerivationBuilder, DerivationInboundChannels, EngineActorRequest, InteropMode,
    L1WatcherActor, QueuedDerivationEngineClient, QueuedL1WatcherEngineClient,
};
use kona_protocol::BlockInfo;
use kona_providers_alloy::OnlineBeaconClient;
use kona_rpc::L1WatcherQueries;
use op_alloy_network::Optimism;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use url::Url;

/// Type alias for the block stream used by L1WatcherActor.
type BoxedBlockStream = std::pin::Pin<Box<dyn futures::Stream<Item = BlockInfo> + Send>>;

mod cli;

use cli::UnifiedCli;

/// Poll interval for L1 latest block stream (seconds).
const HEAD_STREAM_POLL_INTERVAL: u64 = 4;

/// Poll interval for L1 finalized block stream (seconds).
const FINALIZED_STREAM_POLL_INTERVAL: u64 = 60;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    // Initialize versioning so logs/telemetry report the right build info
    base_cli_utils::Version::init();

    // Parse CLI arguments
    let unified_cli = UnifiedCli::parse();

    // Run the unified node
    if let Err(e) = run_unified_node(unified_cli) {
        error!("Unified node failed: {e}");
        std::process::exit(1);
    }
}

fn run_unified_node(_unified_cli: UnifiedCli) -> Result<()> {
    // Parse the reth CLI (using the standard OP CLI parser)
    // This gives us access to the chain spec and other reth configuration
    let cli = Cli::<OpChainSpecParser, cli::UnifiedCli>::parse();

    // Run the OP node with our unified configuration
    cli.run(|builder, args| async move {
        info!("Starting Base Unified Node");

        // Get the chain spec for creating the rollup config
        let chain_spec = builder.config().chain.clone();
        let chain_id = chain_spec.chain().id();
        info!("Chain ID: {}", chain_id);

        // Phase 1: Launch reth EL with unified extension
        info!("Phase 1: Launching reth execution layer...");
        let mut runner = BaseNodeRunner::new(args.rollup_args.clone());

        // Create engine components channel and install extension (if not EL-only)
        let engine_rx = if !args.el_only {
            let (engine_tx, engine_rx) = oneshot::channel::<EngineComponents>();
            runner
                .install_ext::<UnifiedExtension>(UnifiedConfig { engine_components_tx: engine_tx });
            Some(engine_rx)
        } else {
            None
        };

        let node_handle = runner.run(builder);

        // Phase 2-4: Set up and start the consensus layer (if not EL-only mode)
        if let Some(engine_rx) = engine_rx {
            info!("Phase 2: Waiting for engine components...");

            // Create rollup config from chain spec or file
            let rollup_config = create_rollup_config(&args, chain_id)?;
            let rollup_config = Arc::new(rollup_config);
            info!("  L2 Chain ID: {}", rollup_config.l2_chain_id);

            // Get L1 endpoints
            let l1_rpc_url = args.l1_rpc_url.clone();
            let l1_beacon_url = args.l1_beacon_url.clone();

            // Spawn task for CL integration
            tokio::spawn(async move {
                match engine_rx.await {
                    Ok(components) => {
                        info!("Phase 3: Creating InProcessEngineDriver and starting CL...");

                        // Create the in-process engine driver with extracted components
                        let engine_driver = Arc::new(InProcessEngineDriver::new(
                            components.engine_handle,
                            components.payload_store,
                            components.provider,
                        ));

                        info!("Engine components extracted:");
                        info!("  ✓ ConsensusEngineHandle - for forkchoice updates");
                        info!("  ✓ PayloadStore - for payload retrieval");
                        info!("  ✓ Provider - for chain state queries");

                        // Build and start the unified rollup node
                        let node = UnifiedRollupNode::builder()
                            .config(rollup_config.clone())
                            .engine_driver(engine_driver)
                            .build();

                        info!("Starting UnifiedRollupNode with DirectEngineActor...");

                        // Start the node and get engine clients for wiring additional actors
                        match node.start_with_clients().await {
                            Ok((clients, handle)) => {
                                info!("  ✓ DirectEngineActor - running");

                                // Phase 4: Wire up additional actors
                                if let (Some(l1_rpc), Some(l1_beacon)) =
                                    (l1_rpc_url.as_ref(), l1_beacon_url.as_ref())
                                {
                                    info!("Phase 4: Wiring up L1 actors...");
                                    info!("  L1 RPC: {}", l1_rpc);
                                    info!("  L1 Beacon: {}", l1_beacon);

                                    if let Err(e) = wire_l1_actors(
                                        l1_rpc,
                                        l1_beacon,
                                        rollup_config.clone(),
                                        clients.engine_tx.clone(),
                                        handle.cancellation_token().clone(),
                                    )
                                    .await
                                    {
                                        error!("Failed to wire L1 actors: {e}");
                                    }
                                } else {
                                    info!("Phase 4: L1 endpoints not configured");
                                    info!("  To enable full derivation, provide:");
                                    info!("    --l1-rpc-url <URL>");
                                    info!("    --l1-beacon-url <URL>");
                                }

                                info!("");
                                info!("Unified node is operational");

                                // Keep the clients alive and wait for shutdown
                                let _clients = clients;
                                if let Err(e) = handle.wait().await {
                                    error!("UnifiedRollupNode error: {e}");
                                }
                            }
                            Err(e) => {
                                error!("Failed to start UnifiedRollupNode: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to receive engine components: {e}");
                    }
                }
            });
        } else {
            warn!("Running in EL-only mode - consensus layer disabled");
        }

        // Wait for the EL node to complete (or shutdown signal)
        info!("Unified node running. Press Ctrl+C to stop.");
        node_handle.await
    })?;

    Ok(())
}

/// Wires up the L1 actors: DerivationActor and L1WatcherActor.
///
/// This creates the necessary providers, channels, and actors for L1 monitoring
/// and L2 derivation.
async fn wire_l1_actors(
    l1_rpc_url: &str,
    l1_beacon_url: &str,
    rollup_config: Arc<RollupConfig>,
    engine_tx: mpsc::Sender<EngineActorRequest>,
    cancellation: CancellationToken,
) -> Result<()> {
    // Create L1 RPC provider
    let l1_url = Url::parse(l1_rpc_url)?;
    let l1_provider = RootProvider::new_http(l1_url);
    info!("  ✓ L1 RPC provider created");

    // Create L1 beacon client
    let l1_beacon = OnlineBeaconClient::new_http(l1_beacon_url.to_string());
    info!("  ✓ L1 beacon client created");

    // Create L2 provider (connects to our local reth node)
    // For unified binary, we use localhost since L2 state comes from the in-process reth node
    let l2_url = Url::parse("http://localhost:8545")?;
    let l2_provider: RootProvider<Optimism> = RootProvider::new_http(l2_url);

    // Create the DerivationBuilder with all required configuration
    let l1_chain_config = Arc::new(kona_genesis::L1ChainConfig::default());
    let derivation_builder = DerivationBuilder {
        l1_provider: l1_provider.clone(),
        l1_trust_rpc: true,
        l1_beacon: l1_beacon.clone(),
        l2_provider,
        l2_trust_rpc: true,
        rollup_config: rollup_config.clone(),
        l1_config: l1_chain_config,
        interop_mode: InteropMode::Polled,
    };
    info!("  ✓ DerivationBuilder created (polled mode)");

    // Create the DerivationActor with its engine client
    let derivation_engine_client =
        QueuedDerivationEngineClient { engine_actor_request_tx: engine_tx.clone() };

    let (derivation_channels, derivation_actor) =
        DerivationActor::new(derivation_engine_client, cancellation.clone(), derivation_builder);
    info!("  ✓ DerivationActor created");

    // Spawn the DerivationActor
    let derivation_cancellation = cancellation.clone();
    tokio::spawn(async move {
        info!("DerivationActor starting...");
        if let Err(e) = derivation_actor.start(()).await
            && !derivation_cancellation.is_cancelled()
        {
            error!("DerivationActor error: {e}");
        }
        info!("DerivationActor stopped");
    });
    info!("  ✓ DerivationActor spawned");

    // Create the L1WatcherActor using the derivation channels
    let l1_watcher_result = create_l1_watcher(
        l1_provider,
        rollup_config.clone(),
        derivation_channels,
        engine_tx,
        cancellation.clone(),
    )
    .await;

    match l1_watcher_result {
        Ok(l1_watcher) => {
            let watcher_cancellation = cancellation.clone();
            tokio::spawn(async move {
                info!("L1WatcherActor starting...");
                if let Err(e) = l1_watcher.start(()).await
                    && !watcher_cancellation.is_cancelled()
                {
                    error!("L1WatcherActor error: {e}");
                }
                info!("L1WatcherActor stopped");
            });
            info!("  ✓ L1WatcherActor spawned");
        }
        Err(e) => {
            warn!("Failed to create L1WatcherActor: {e}");
            warn!("  Derivation will wait for external L1 head updates");
        }
    }

    info!("L1 actors wired successfully");
    Ok(())
}

/// Creates the L1WatcherActor with block streams.
async fn create_l1_watcher(
    l1_provider: RootProvider,
    rollup_config: Arc<RollupConfig>,
    derivation_channels: DerivationInboundChannels,
    engine_tx: mpsc::Sender<EngineActorRequest>,
    cancellation: CancellationToken,
) -> Result<L1WatcherActor<BoxedBlockStream, RootProvider, QueuedL1WatcherEngineClient>> {
    // Create query channel (not used in unified binary, but required by L1WatcherActor)
    let (_l1_query_tx, l1_query_rx) = mpsc::channel::<L1WatcherQueries>(1024);

    // Create signer channel (for unsafe block signer updates from system config)
    let (signer_tx, mut signer_rx) = mpsc::channel::<alloy_primitives::Address>(16);

    // Spawn a task to log signer updates (in full impl, this would go to NetworkActor)
    tokio::spawn(async move {
        while let Some(signer) = signer_rx.recv().await {
            debug!("L1 system config updated unsafe block signer: {}", signer);
        }
    });

    // The L1WatcherActor sends finalized L1 blocks directly to the engine via its engine client.
    let engine_client = QueuedL1WatcherEngineClient { engine_actor_request_tx: engine_tx };

    // Create block streams using polling
    let head_stream: BoxedBlockStream = Box::pin(create_block_stream(
        l1_provider.clone(),
        BlockNumberOrTag::Latest,
        Duration::from_secs(HEAD_STREAM_POLL_INTERVAL),
    ));

    let finalized_stream: BoxedBlockStream = Box::pin(create_block_stream(
        l1_provider.clone(),
        BlockNumberOrTag::Finalized,
        Duration::from_secs(FINALIZED_STREAM_POLL_INTERVAL),
    ));

    // L1 head updates go to the DerivationActor via its inbound channels
    let l1_head_updates_tx = derivation_channels.l1_head_updates_tx;

    let l1_watcher = L1WatcherActor::new(
        rollup_config,
        l1_provider,
        l1_query_rx,
        l1_head_updates_tx,
        engine_client,
        signer_tx,
        cancellation,
        head_stream,
        finalized_stream,
    );

    Ok(l1_watcher)
}

/// Creates a block stream that polls for new blocks at the specified interval.
fn create_block_stream(
    provider: RootProvider,
    tag: BlockNumberOrTag,
    poll_interval: Duration,
) -> impl futures::Stream<Item = BlockInfo> + Unpin + Send {
    let last_block_hash = None;

    futures::stream::unfold(
        (provider, tag, poll_interval, last_block_hash),
        move |(provider, tag, interval, mut last_hash)| async move {
            loop {
                tokio::time::sleep(interval).await;

                match provider.get_block_by_number(tag).await {
                    Ok(Some(block)) => {
                        let block_hash = block.header.hash;

                        // Only yield if block changed (deduplication)
                        if last_hash != Some(block_hash) {
                            last_hash = Some(block_hash);

                            let block_info = BlockInfo {
                                hash: block_hash,
                                number: block.header.number,
                                parent_hash: block.header.parent_hash,
                                timestamp: block.header.timestamp,
                            };

                            return Some((block_info, (provider, tag, interval, last_hash)));
                        }
                    }
                    Ok(None) => {
                        debug!("No block found for tag {:?}", tag);
                    }
                    Err(e) => {
                        debug!("Error fetching block: {e}");
                    }
                }
            }
        },
    )
    .boxed()
}

/// Creates a rollup config from CLI arguments and chain spec.
fn create_rollup_config(args: &UnifiedCli, chain_id: u64) -> Result<RollupConfig> {
    if let Some(ref config_path) = args.rollup_config {
        // Load from file
        let config_json = std::fs::read_to_string(config_path)?;
        let config: RollupConfig = serde_json::from_str(&config_json)?;
        info!("Loaded rollup config from {:?}", config_path);
        Ok(config)
    } else {
        // Create a minimal config for testing
        // In production, this should always come from a file
        warn!("No rollup config specified, using minimal default");
        Ok(RollupConfig { l2_chain_id: chain_id.into(), ..Default::default() })
    }
}
