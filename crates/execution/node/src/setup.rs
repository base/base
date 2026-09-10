//! Helpers for setting up parts of the node.

use std::sync::Arc;

use alloy_primitives::{B256, BlockNumber};
use base_common_observability_tracing::tracing::debug;
use base_common_runtime_tasks::TaskExecutor;
use base_common_types_chain::BaseBlock;
use base_execution_evm_blocks::BaseBeaconConsensus;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_state_maintenance::StaticFileProducer;
use base_execution_state_provider::ProviderFactory;
use reth_downloaders::{
    bodies::bodies::BodiesDownloaderBuilder,
    headers::reverse_headers::ReverseHeadersDownloaderBuilder,
};
use reth_exex::ExExManagerHandle;
use reth_stages::{Pipeline, StageId, StageSet, prelude::DefaultStages, stages::ExecutionStage};
use tokio::sync::watch;
use {
    base_execution_network_service::BodyDownloader,
    base_execution_network_service::HeaderDownloader, base_execution_network_wire::BlockClient,
};
use {base_execution_state_types::PruneConfig, reth_config::config::StageConfig};

/// Constructs a [Pipeline] that's wired to the network
#[expect(clippy::too_many_arguments)]
pub fn build_networked_pipeline<Client>(
    config: &StageConfig,
    client: Client,
    consensus: Arc<BaseBeaconConsensus>,
    provider_factory: ProviderFactory,
    task_executor: &TaskExecutor,
    metrics_tx: reth_stages::MetricEventsSender,
    prune_config: PruneConfig,
    max_block: Option<BlockNumber>,
    static_file_producer: StaticFileProducer<ProviderFactory>,
    evm_config: BaseEvmConfig,
    exex_manager_handle: ExExManagerHandle,
    disabled_stages: &[StageId],
) -> eyre::Result<Pipeline>
where
    Client: BlockClient + 'static,
{
    // building network downloaders using the fetch client
    let header_downloader = ReverseHeadersDownloaderBuilder::new(config.headers)
        .build(client.clone(), consensus.clone())
        .into_task_with(task_executor);

    let body_downloader = BodiesDownloaderBuilder::new(config.bodies)
        .build(client, consensus.clone(), provider_factory.clone())
        .into_task_with(task_executor);

    let pipeline = build_pipeline(
        provider_factory,
        config,
        header_downloader,
        body_downloader,
        consensus,
        max_block,
        metrics_tx,
        prune_config,
        static_file_producer,
        evm_config,
        exex_manager_handle,
        disabled_stages,
    )?;

    Ok(pipeline)
}

/// Builds the [Pipeline] with the given [`ProviderFactory`] and downloaders.
#[expect(clippy::too_many_arguments)]
pub fn build_pipeline<H, B>(
    provider_factory: ProviderFactory,
    stage_config: &StageConfig,
    header_downloader: H,
    body_downloader: B,
    consensus: Arc<BaseBeaconConsensus>,
    max_block: Option<u64>,
    metrics_tx: reth_stages::MetricEventsSender,
    prune_config: PruneConfig,
    static_file_producer: StaticFileProducer<ProviderFactory>,
    evm_config: BaseEvmConfig,
    exex_manager_handle: ExExManagerHandle,
    disabled_stages: &[StageId],
) -> eyre::Result<Pipeline>
where
    H: HeaderDownloader + 'static,
    B: BodyDownloader<Block = BaseBlock> + 'static,
{
    let mut builder = Pipeline::builder();

    if let Some(max_block) = max_block {
        debug!(target: "reth::cli", max_block, "Configuring builder to use max block");
        builder = builder.with_max_block(max_block)
    }

    let (tip_tx, tip_rx) = watch::channel(B256::ZERO);

    let pipeline = builder
        .with_tip_sender(tip_tx)
        .with_metrics_tx(metrics_tx)
        .add_stages(
            DefaultStages::new(
                provider_factory.clone(),
                tip_rx,
                Arc::clone(&consensus),
                header_downloader,
                body_downloader,
                evm_config.clone(),
                stage_config.clone(),
                prune_config.segments,
            )
            .set(ExecutionStage::new(
                evm_config,
                consensus,
                stage_config.execution.into(),
                stage_config.execution_external_clean_threshold(),
                exex_manager_handle,
            ))
            .disable_all(disabled_stages),
        )
        .build(provider_factory, static_file_producer);

    Ok(pipeline)
}
