//! OP Stack Engine API builder.

use std::sync::Arc;

use alloy_rpc_types_engine::ClientVersionV1;
use reth_node_api::{AddOnsContext, EngineApiValidator, NodeTypes};
use reth_node_builder::rpc::{EngineApiBuilder, PayloadValidatorBuilder};
use reth_node_core::version::{CLIENT_CODE, version_metadata};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::OpEngineTypes;
pub(crate) use reth_optimism_rpc::OpEngineApi;
use reth_optimism_rpc::engine::OP_ENGINE_CAPABILITIES;
use reth_payload_builder::PayloadStore;
use reth_rpc_engine_api::EngineCapabilities;

use crate::traits::NodeComponents;

/// Builder for [`OpEngineApi`] implementation.
///
/// This builder creates the standard OP Stack engine API using reth's
/// [`OpEngineApi`] directly, which implements [`OpEngineApiServer`].
///
/// [`OpEngineApiServer`]: reth_optimism_rpc::OpEngineApiServer
#[derive(Debug, Clone)]
pub struct OpEngineApiBuilder<EV> {
    engine_validator_builder: EV,
}

impl<EV> Default for OpEngineApiBuilder<EV>
where
    EV: Default,
{
    fn default() -> Self {
        Self { engine_validator_builder: EV::default() }
    }
}

impl<N, EV> EngineApiBuilder<N> for OpEngineApiBuilder<EV>
where
    N: NodeComponents,
    EV: PayloadValidatorBuilder<N>,
    EV::Validator: EngineApiValidator<<N::Types as NodeTypes>::Payload>,
{
    type EngineApi = OpEngineApi<N::Provider, OpEngineTypes, N::Pool, EV::Validator, OpChainSpec>;

    async fn build_engine_api(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::EngineApi> {
        let engine_validator = self.engine_validator_builder.build(ctx).await?;
        let client = ClientVersionV1 {
            code: CLIENT_CODE,
            name: version_metadata().name_client.to_string(),
            version: version_metadata().cargo_pkg_version.to_string(),
            commit: version_metadata().vergen_git_sha.to_string(),
        };
        let inner = reth_rpc_engine_api::EngineApi::new(
            ctx.node.provider().clone(),
            Arc::clone(&ctx.config.chain),
            ctx.beacon_engine_handle.clone(),
            PayloadStore::new(ctx.node.payload_builder_handle().clone()),
            ctx.node.pool().clone(),
            Box::new(ctx.node.task_executor().clone()),
            client,
            EngineCapabilities::new(OP_ENGINE_CAPABILITIES.iter().copied()),
            engine_validator,
            ctx.config.engine.accept_execution_requests_hash,
            ctx.node.network().clone(),
        );

        Ok(OpEngineApi::new(inner))
    }
}
