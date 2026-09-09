//! Built-in consensus debugging and local mining services.
use std::sync::Arc;

use base_common_network::Base;
use base_execution_payload_types::BaseBuiltPayload;
use reth_consensus_debug_client::{DebugConsensusClient, EtherscanBlockProvider, RpcBlockProvider};
use reth_engine_local::LocalMiner;
use reth_primitives_traits::SealedBlock;
use tracing::info;

use crate::NodeHandle;

/// Starts the Base debug services selected by operational CLI arguments.
#[derive(Debug)]
pub struct BaseDebugServices;
impl BaseDebugServices {
    /// Starts the configured debug consensus source and local miner.
    pub async fn start(handle: &NodeHandle) -> eyre::Result<()> {
        let config = &handle.node.config;

        if let Some(url) = config.debug.rpc_consensus_url.clone() {
            info!(target: "reth::cli", url = %url, "Using RPC consensus client");

            let block_provider =
                RpcBlockProvider::<Base, _>::new(url.as_str(), move |block_response, extras| {
                    let primitive_block = block_response
                        .map_transactions(|tx| tx.inner.inner.into_inner())
                        .into_consensus();
                    BaseBuiltPayload::block_to_payload(
                        SealedBlock::new_unhashed(primitive_block),
                        extras.bal,
                    )
                })
                .await?;

            let rpc_consensus_client = DebugConsensusClient::new(
                handle.node.execution.driver.clone(),
                Arc::new(block_provider),
            );

            handle.node.task_executor.spawn_critical_task("rpc-ws consensus client", async move {
                rpc_consensus_client.run().await
            });
        } else if let Some(maybe_custom_etherscan_url) = config.debug.etherscan.clone() {
            info!(target: "reth::cli", "Using etherscan as consensus client");

            let chain = config.chain.chain();
            let etherscan_url = maybe_custom_etherscan_url.map(Ok).unwrap_or_else(|| {
                chain
                    .etherscan_urls()
                    .map(|urls| urls.0.to_string())
                    .ok_or_else(|| eyre::eyre!("failed to get etherscan url for chain: {chain}"))
            })?;

            let block_provider = EtherscanBlockProvider::new(
                etherscan_url,
                chain.etherscan_api_key().ok_or_else(|| {
                    eyre::eyre!(
                        "etherscan api key not found for rpc consensus client for chain: {chain}"
                    )
                })?,
                chain.id(),
                move |rpc_block: base_common_rpc_types::Block<
                    base_common_consensus::BaseTxEnvelope,
                >| {
                    let primitive_block = rpc_block.into_consensus();
                    BaseBuiltPayload::block_to_payload(
                        SealedBlock::new_unhashed(primitive_block),
                        None,
                    )
                },
            );
            let rpc_consensus_client = DebugConsensusClient::new(
                handle.node.execution.driver.clone(),
                Arc::new(block_provider),
            );
            handle
                .node
                .task_executor
                .spawn_critical_task("etherscan consensus client", async move {
                    rpc_consensus_client.run().await
                });
        }

        if config.dev.dev {
            info!(target: "reth::cli", "Using local payload attributes builder for dev mode");

            let blockchain_db = handle.node.provider.clone();
            let chain_spec = config.chain.clone();
            let beacon_engine_handle = handle.node.execution.driver.clone();
            let pool = handle.node.pool.clone();
            let payload_builder_handle = handle.node.payload_builder_handle.clone();

            let builder = crate::BaseLocalPayloadAttributesBuilder::new(chain_spec);
            let dev_mining_mode = config.dev_mining_mode(pool);
            let finality_depth = config.dev.finality_depth;
            let payload_wait_time = config.dev.payload_wait_time;
            if let (Some(wait_time), Some(block_time)) = (payload_wait_time, config.dev.block_time)
            {
                eyre::ensure!(
                    wait_time <= block_time,
                    "--dev.payload-wait-time ({wait_time:?}) must be <= --dev.block-time ({block_time:?})"
                );
            }
            handle.node.task_executor.spawn_critical_task("local engine", async move {
                LocalMiner::new(
                    blockchain_db,
                    builder,
                    beacon_engine_handle,
                    dev_mining_mode,
                    payload_builder_handle,
                )
                .with_finality_depth(finality_depth)
                .with_payload_wait_time_opt(payload_wait_time)
                .run()
                .await
            });
        }

        Ok(())
    }
}
