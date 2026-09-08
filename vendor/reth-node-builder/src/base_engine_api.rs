//! RPC components for Base node launch.
//!
//! `BaseEthApiBuilder` constructs the `eth` API from the node's provider, pool, and
//! execution configuration. The add-ons below attach the engine API, payload validator,
//! and RPC extensions to that shared node context.

use std::sync::Arc;

use alloy_rpc_types::engine::ClientVersionV1;
use base_execution_payload_builder::{BaseEngineValidator, PayloadStore};
use base_execution_rpc::BaseEngineApi;
use reth_node_api::{AddOnsContext, FullNodeComponents};
use reth_node_core::version::{CLIENT_CODE, version_metadata};
use reth_trie_common::KeccakKeyHasher;

/// Builder for basic [`BaseEngineApi`] implementation.
#[derive(Debug, Default, Clone)]
pub struct BaseEngineApiBuilder;

impl BaseEngineApiBuilder {
    /// Constructs the Base engine API with its fixed validation and capabilities.
    pub fn build_engine_api<N: FullNodeComponents>(
        ctx: &AddOnsContext<'_, N>,
    ) -> BaseEngineApi<N::Provider, reth_node_api::BaseNodePool<N::Provider>, BaseEngineValidator>
    {
        let engine_validator =
            BaseEngineValidator::new::<KeccakKeyHasher>(Arc::clone(&ctx.config.chain));
        let client = ClientVersionV1 {
            code: CLIENT_CODE,
            name: "Base-Reth".to_string(),
            version: version_metadata().cargo_pkg_version.to_string(),
            commit: version_metadata().vergen_git_sha.to_string(),
        };
        BaseEngineApi::new(
            ctx.node.provider().clone(),
            Arc::clone(&ctx.config.chain),
            ctx.beacon_engine_handle.clone(),
            PayloadStore::new(ctx.node.payload_builder_handle().clone()),
            ctx.node.pool().clone(),
            ctx.node.task_executor().clone(),
            client,
            engine_validator,
            ctx.config.engine.accept_execution_requests_hash,
            ctx.node.network().clone(),
        )
    }
}
