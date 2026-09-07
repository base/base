//! RPC components for Base node launch.
//!
//! `BaseEthApiBuilder` constructs the `eth` API from the node's provider, pool, and
//! execution configuration. The add-ons below attach the engine API, payload validator,
//! and RPC extensions to that shared node context.

use std::sync::Arc;

use alloy_rpc_types_engine::ClientVersionV1;
use base_common_consensus::BaseTxEnvelope;
use base_execution_payload_builder::BaseEngineValidator;
use base_execution_rpc::{BaseEngineApi, engine::ENGINE_CAPABILITIES};
use reth_node_api::{AddOnsContext, FullNodeComponents};
use reth_node_builder::rpc::EngineApiBuilder;
use reth_node_core::version::{CLIENT_CODE, version_metadata};
use reth_payload_builder::PayloadStore;
use reth_rpc_engine_api::{EngineApi, EngineCapabilities};
use reth_trie_common::KeccakKeyHasher;

use crate::CLIENT_NAME;

/// Builder for basic [`BaseEngineApi`] implementation.
#[derive(Debug, Default, Clone)]
pub struct BaseEngineApiBuilder;

impl<N> EngineApiBuilder<N> for BaseEngineApiBuilder
where
    N: FullNodeComponents,
{
    type EngineApi = BaseEngineApi<N::Provider, N::Pool, BaseEngineValidator<BaseTxEnvelope>>;

    async fn build_engine_api(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::EngineApi> {
        let engine_validator =
            BaseEngineValidator::new::<KeccakKeyHasher>(Arc::clone(&ctx.config.chain));
        let client = ClientVersionV1 {
            code: CLIENT_CODE,
            name: CLIENT_NAME.to_string(),
            version: version_metadata().cargo_pkg_version.to_string(),
            commit: version_metadata().vergen_git_sha.to_string(),
        };
        let inner = EngineApi::new(
            ctx.node.provider().clone(),
            Arc::clone(&ctx.config.chain),
            ctx.beacon_engine_handle.clone(),
            PayloadStore::new(ctx.node.payload_builder_handle().clone()),
            ctx.node.pool().clone(),
            ctx.node.task_executor().clone(),
            client,
            EngineCapabilities::new(ENGINE_CAPABILITIES.iter().copied()),
            engine_validator,
            ctx.config.engine.accept_execution_requests_hash,
            ctx.node.network().clone(),
        );

        Ok(BaseEngineApi::new(inner))
    }
}
