use std::{fmt::Debug, sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::RootProvider;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_consensus_engine::{BaseEngineClient, EngineClient};
use base_consensus_rpc::RpcBuilder;
use tokio_util::sync::CancellationToken;

use crate::{
    NodeActor, ShutdownSignal,
    follow::{
        engine::EngineApiFollowEngine,
        error::FollowError,
        local::{FollowLocalClient, LocalL2Client},
        proof_gate::{ActiveProofGate, NoopProofGate, ProofGate},
        rpc::FollowRpcActor,
        runtime::FollowRuntime,
        source::RemoteL2Client,
    },
};

/// A lightweight node that follows another L2 node by fetching source L2
/// payloads and inserting them into the local execution engine.
#[derive(Debug)]
pub struct FollowNode<E = BaseEngineClient<RootProvider, RootProvider<Base>>>
where
    E: EngineClient + Debug + 'static,
{
    config: Arc<RollupConfig>,
    engine_client: Arc<E>,
    local_l2_provider: RootProvider<Base>,
    l2_source: RemoteL2Client,
    proofs_enabled: bool,
    proofs_max_blocks_ahead: u64,
    insert_delay: Duration,
    rpc_builder: RpcBuilder,
}

impl<E> FollowNode<E>
where
    E: EngineClient + Debug + 'static,
{
    /// Creates a new [`FollowNode`].
    pub const fn new(
        config: Arc<RollupConfig>,
        engine_client: Arc<E>,
        local_l2_provider: RootProvider<Base>,
        l2_source: RemoteL2Client,
        rpc_builder: RpcBuilder,
        proofs_enabled: bool,
        proofs_max_blocks_ahead: u64,
        insert_delay: Duration,
    ) -> Self {
        Self {
            config,
            engine_client,
            local_l2_provider,
            l2_source,
            rpc_builder,
            proofs_enabled,
            proofs_max_blocks_ahead,
            insert_delay,
        }
    }

    /// Starts the follow node.
    pub async fn start(&self) -> Result<(), FollowError> {
        let cancellation = CancellationToken::new();
        let local =
            Arc::new(LocalL2Client::new(self.local_l2_provider.clone(), Arc::clone(&self.config)));
        let latest = local
            .block_info(BlockNumberOrTag::Latest)
            .await?
            .ok_or(FollowError::LocalBlockUnavailable(BlockNumberOrTag::Latest))?;
        let safe = local.block_info(BlockNumberOrTag::Safe).await?.unwrap_or_default();
        let finalized = local.block_info(BlockNumberOrTag::Finalized).await?.unwrap_or_default();
        let engine = Arc::new(EngineApiFollowEngine::new(
            Arc::clone(&self.engine_client),
            Arc::clone(&self.config),
            latest,
            safe,
            finalized,
        ));
        let rpc = FollowRpcActor::new(self.rpc_builder.clone(), Arc::clone(&local));

        if self.proofs_enabled {
            let proof_gate =
                ActiveProofGate::new(Arc::clone(&local), self.proofs_max_blocks_ahead).await?;
            self.start_runtime(local, engine, latest, proof_gate, rpc, cancellation).await
        } else {
            self.start_runtime(local, engine, latest, NoopProofGate, rpc, cancellation).await
        }
    }

    async fn start_runtime<Gate>(
        &self,
        local: Arc<LocalL2Client>,
        engine: Arc<EngineApiFollowEngine<E>>,
        latest: base_protocol::L2BlockInfo,
        proof_gate: Gate,
        rpc: FollowRpcActor<LocalL2Client>,
        cancellation: CancellationToken,
    ) -> Result<(), FollowError>
    where
        Gate: ProofGate + 'static,
    {
        let runtime = FollowRuntime::new(
            Arc::clone(&local),
            Arc::new(self.l2_source.clone()),
            engine,
            cancellation.clone(),
            latest,
            proof_gate,
            self.insert_delay,
        );

        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(runtime.start());
        tasks.spawn(rpc.start(cancellation.clone()));

        tokio::select! {
            result = tasks.join_next() => {
                cancellation.cancel();
                if let Some(result) = result {
                    result??;
                }
            }
            _ = ShutdownSignal::wait() => {
                cancellation.cancel();
            }
        }
        Ok(())
    }
}
