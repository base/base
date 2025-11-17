use std::sync::Arc;

use aws_config::BehaviorVersion;
use aws_sdk_s3::Client as S3Client;
use base_reth_flashblocks_rpc::{
    rpc::{EthApiExt, EthApiOverrideServer},
    state::FlashblocksState,
    subscription::FlashblocksSubscriber,
};
use base_reth_metering::{MeteringApiImpl, MeteringApiServer};
use base_reth_transaction_status::{
    TransactionStatusApiImpl, TransactionStatusApiServer, TransactionStatusProxyImpl,
};
use base_reth_transaction_tracing::transaction_tracing_exex;
use clap::Parser;
use futures_util::TryStreamExt;
use once_cell::sync::OnceCell;
use reth::{
    builder::{EngineNodeLauncher, Node, NodeHandle, TreeConfig},
    providers::providers::BlockchainProvider,
    version::{RethCliVersionConsts, default_reth_version_metadata, try_init_version_metadata},
};
use reth_exex::ExExEvent;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::{OpNode, args::RollupArgs};
use tracing::info;
use url::Url;

pub const NODE_RETH_CLIENT_VERSION: &str = concat!("base/v", env!("CARGO_PKG_VERSION"));

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
struct Args {
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    #[arg(long = "websocket-url", value_name = "WEBSOCKET_URL")]
    pub websocket_url: Option<String>,

    #[arg(
        long = "max-pending-blocks-depth",
        value_name = "MAX_PENDING_BLOCKS_DEPTH",
        default_value = "3"
    )]
    pub max_pending_blocks_depth: u64,

    /// Enable transaction tracing ExEx for mempool-to-block timing analysis
    #[arg(long = "enable-transaction-tracing", value_name = "ENABLE_TRANSACTION_TRACING")]
    pub enable_transaction_tracing: bool,

    /// Enable `info` logs for transaction tracing
    #[arg(
        long = "enable-transaction-tracing-logs",
        value_name = "ENABLE_TRANSACTION_TRACING_LOGS"
    )]
    pub enable_transaction_tracing_logs: bool,

    /// Enable metering RPC for transaction bundle simulation
    #[arg(long = "enable-metering", value_name = "ENABLE_METERING")]
    pub enable_metering: bool,

    /// Enable transaction status RPC for transaction status lookup
    #[arg(long = "enable-transaction-status", value_name = "ENABLE_TRANSACTION_STATUS")]
    pub enable_transaction_status: bool,

    /// S3 bucket for transaction status lookup
    #[arg(long = "transaction-status-bucket", value_name = "TRANSACTION_STATUS_BUCKET")]
    pub transaction_status_bucket: String,

    /// Enable transaction status proxying to external endpoint
    #[arg(
        long = "enable-transaction-status-proxy",
        value_name = "ENABLE_TRANSACTION_STATUS_PROXY"
    )]
    pub enable_transaction_status_proxy: bool,

    /// External endpoint URL for transaction status proxying
    /// Mainnet: https://mainnet.base.org
    /// Sepolia: https://sepolia.base.org
    #[arg(long = "transaction-status-proxy-url", value_name = "TRANSACTION_STATUS_PROXY_URL")]
    pub transaction_status_proxy_url: Option<String>,
}

impl Args {
    fn flashblocks_enabled(&self) -> bool {
        self.websocket_url.is_some()
    }
}

fn main() {
    let default_version_metadata = default_reth_version_metadata();
    try_init_version_metadata(RethCliVersionConsts {
        name_client: "Base Reth Node".to_string().into(),
        cargo_pkg_version: format!(
            "{}/{}",
            default_version_metadata.cargo_pkg_version,
            env!("CARGO_PKG_VERSION")
        )
        .into(),
        p2p_client_version: format!(
            "{}/{}",
            default_version_metadata.p2p_client_version, NODE_RETH_CLIENT_VERSION
        )
        .into(),
        extra_data: format!("{}/{}", default_version_metadata.extra_data, NODE_RETH_CLIENT_VERSION)
            .into(),
        ..default_version_metadata
    })
    .expect("Unable to init version metadata");

    Cli::<OpChainSpecParser, Args>::parse()
        .run(|builder, args| async move {
            info!(message = "starting custom Base node");

            let flashblocks_enabled = args.flashblocks_enabled();
            let transaction_tracing_enabled = args.enable_transaction_tracing;
            let metering_enabled = args.enable_metering;
            let op_node = OpNode::new(args.rollup_args.clone());

            let fb_cell: Arc<OnceCell<Arc<FlashblocksState<_>>>> = Arc::new(OnceCell::new());

            let transaction_status_enabled = args.enable_transaction_status;
            let transaction_status_proxy_enabled = args.enable_transaction_status_proxy;
            let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
            let s3_client = S3Client::new(&config);

            let NodeHandle { node: _node, node_exit_future } = builder
                .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
                .with_components(op_node.components())
                .with_add_ons(op_node.add_ons())
                .on_component_initialized(move |_ctx| Ok(()))
                .install_exex_if(
                    transaction_tracing_enabled,
                    "transaction-tracing",
                    move |ctx| async move {
                        Ok(transaction_tracing_exex(ctx, args.enable_transaction_tracing_logs))
                    },
                )
                .install_exex_if(flashblocks_enabled, "flashblocks-canon", {
                    let fb_cell = fb_cell.clone();
                    move |mut ctx| async move {
                        let fb = fb_cell
                            .get_or_init(|| {
                                Arc::new(FlashblocksState::new(
                                    ctx.provider().clone(),
                                    args.max_pending_blocks_depth,
                                ))
                            })
                            .clone();
                        Ok(async move {
                            while let Some(note) = ctx.notifications.try_next().await? {
                                if let Some(committed) = note.committed_chain() {
                                    for b in committed.blocks_iter() {
                                        fb.on_canonical_block_received(b);
                                    }
                                    let _ = ctx.events.send(ExExEvent::FinishedHeight(
                                        committed.tip().num_hash(),
                                    ));
                                }
                            }
                            Ok(())
                        })
                    }
                })
                .extend_rpc_modules(move |ctx| {
                    if metering_enabled {
                        info!(message = "Starting Metering RPC");
                        let metering_api = MeteringApiImpl::new(ctx.provider().clone());
                        ctx.modules.merge_configured(metering_api.into_rpc())?;
                    }

                    if transaction_status_enabled {
                        info!(message = "Starting Transaction Status RPC");

                        // this is for external node users who will need to proxy requests to Base managed
                        // rpc nodes to get transaction status
                        if transaction_status_proxy_enabled {
                            info!(message = "Transaction status proxying enabled");

                            let proxy_api = TransactionStatusProxyImpl::new(
                                args.transaction_status_proxy_url.clone(),
                            )
                            .expect("Failed to create transaction status proxy");

                            ctx.modules.merge_configured(proxy_api.into_rpc())?;
                        } else {
                            let transaction_status_api = TransactionStatusApiImpl::new(
                                s3_client,
                                args.transaction_status_bucket.clone(),
                            );
                            ctx.modules.merge_configured(transaction_status_api.into_rpc())?;
                        }
                    }

                    if flashblocks_enabled {
                        info!(message = "Starting Flashblocks");

                        let ws_url = Url::parse(
                            args.websocket_url
                                .expect("WEBSOCKET_URL must be set when Flashblocks is enabled")
                                .as_str(),
                        )?;

                        let fb = fb_cell
                            .get_or_init(|| {
                                Arc::new(FlashblocksState::new(
                                    ctx.provider().clone(),
                                    args.max_pending_blocks_depth,
                                ))
                            })
                            .clone();
                        fb.start();

                        let mut flashblocks_client = FlashblocksSubscriber::new(fb.clone(), ws_url);
                        flashblocks_client.start();

                        let api_ext = EthApiExt::new(
                            ctx.registry.eth_api().clone(),
                            ctx.registry.eth_handlers().filter.clone(),
                            fb,
                        );

                        ctx.modules.replace_configured(api_ext.into_rpc())?;
                    } else {
                        info!(message = "flashblocks integration is disabled");
                    }
                    Ok(())
                })
                .launch_with_fn(|builder| {
                    let engine_tree_config = TreeConfig::default()
                        .with_persistence_threshold(builder.config().engine.persistence_threshold)
                        .with_memory_block_buffer_target(
                            builder.config().engine.memory_block_buffer_target,
                        );

                    let launcher = EngineNodeLauncher::new(
                        builder.task_executor().clone(),
                        builder.config().datadir(),
                        engine_tree_config,
                    );

                    builder.launch_with(launcher)
                })
                .await?;

            node_exit_future.await
        })
        .unwrap();
}
