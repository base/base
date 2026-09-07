//! RPC components for Base node launch.
//!
//! `BaseEthApiBuilder` constructs the `eth` API from the node's provider, pool, and
//! execution configuration. The add-ons below attach the engine API, payload validator,
//! and RPC extensions to that shared node context.

use std::sync::Arc;

use alloy_rpc_types_engine::ClientVersionV1;
use base_execution_rpc::{BaseEngineApi, engine::ENGINE_CAPABILITIES};
use reth_node_api::{AddOnsContext, EngineApiValidator, FullNodeComponents};
use reth_node_builder::rpc::{EngineApiBuilder, PayloadValidatorBuilder};
use reth_node_core::version::{CLIENT_CODE, version_metadata};
use reth_payload_builder::PayloadStore;
use reth_rpc_engine_api::{EngineApi, EngineCapabilities};

use crate::CLIENT_NAME;

/// Builder for basic [`BaseEngineApi`] implementation.
#[derive(Debug, Default, Clone)]
pub struct BaseEngineApiBuilder<EV> {
    engine_validator_builder: EV,
}

impl<N, EV> EngineApiBuilder<N> for BaseEngineApiBuilder<EV>
where
    N: FullNodeComponents,
    EV: PayloadValidatorBuilder<N>,
    EV::Validator: EngineApiValidator,
{
    type EngineApi = BaseEngineApi<N::Provider, N::Pool, EV::Validator>;

    async fn build_engine_api(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::EngineApi> {
        let Self { engine_validator_builder } = self;

        let engine_validator = engine_validator_builder.build(ctx).await?;
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
