use std::{fmt::Debug, sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::RootProvider;
use base_cli_utils::GenesisStartArgs;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_consensus_engine::{BaseEngineClient, EngineClient};
use base_consensus_rpc::RpcBuilder;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::{
    EngineConfig, NodeActor, ShutdownSignal,
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
    genesis_start: GenesisStartArgs,
    l1_provider: RootProvider,
    local_l2_provider: RootProvider<Base>,
    l2_source: RemoteL2Client,
    proofs_enabled: bool,
    proofs_max_blocks_ahead: u64,
    insert_delay: Duration,
    rpc_builder: Option<RpcBuilder>,
}

/// Runtime dependencies and options for a [`FollowNode`].
#[derive(Debug)]
pub struct FollowNodeConfig<E = BaseEngineClient<RootProvider, RootProvider<Base>>>
where
    E: EngineClient + Debug + 'static,
{
    /// The rollup configuration for the L2 chain.
    pub rollup_config: Arc<RollupConfig>,
    /// Client used to insert payloads into the local execution engine.
    pub engine_client: Arc<E>,
    /// Explicit permission to launch a new chain from genesis.
    pub genesis_start: GenesisStartArgs,
    /// Provider for reading local L2 state.
    pub local_l2_provider: RootProvider<Base>,
    /// Provider used to check L2 block origins against canonical L1.
    pub l1_provider: RootProvider,
    /// Source L2 client used to fetch payloads to follow.
    pub l2_source: RemoteL2Client,
    /// Optional RPC server configuration.
    pub rpc_builder: Option<RpcBuilder>,
    /// Whether to gate sync behind proofs progress.
    pub proofs_enabled: bool,
    /// Maximum blocks the follow node may advance beyond proofs progress.
    pub proofs_max_blocks_ahead: u64,
    /// Delay after each successful source payload insert.
    pub insert_delay: Duration,
}

impl<E> FollowNode<E>
where
    E: EngineClient + Debug + 'static,
{
    /// Creates a new [`FollowNode`].
    pub fn new(config: FollowNodeConfig<E>) -> Self {
        Self {
            config: config.rollup_config,
            engine_client: config.engine_client,
            genesis_start: config.genesis_start,
            l1_provider: config.l1_provider,
            local_l2_provider: config.local_l2_provider,
            l2_source: config.l2_source,
            rpc_builder: config.rpc_builder,
            proofs_enabled: config.proofs_enabled,
            proofs_max_blocks_ahead: config.proofs_max_blocks_ahead,
            insert_delay: config.insert_delay,
        }
    }

    /// Starts the follow node, creating a fresh cancellation token.
    pub async fn start(&self) -> Result<(), FollowError> {
        self.start_with_cancellation(CancellationToken::new()).await
    }

    /// Starts the follow node, driven by an externally supplied cancellation token.
    ///
    /// Cancelling the token stops the follow runtime, which lets an embedding caller (e.g. the
    /// unified binary running execution in-process) coordinate a clean shutdown across both
    /// services. The follow node also stops on an internal [`ShutdownSignal`].
    pub async fn start_with_cancellation(
        &self,
        cancellation: CancellationToken,
    ) -> Result<(), FollowError> {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(()),
            () = ShutdownSignal::wait() => return Ok(()),
            result = EngineConfig::check_startup(self.engine_client.as_ref(), self.genesis_start) => {
                result.map_err(FollowError::Startup)?;
            }
        }
        let local = Arc::new(LocalL2Client::new(
            self.local_l2_provider.clone(),
            self.l1_provider.clone(),
            Arc::clone(&self.config),
        ));
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
        let rpc = self
            .rpc_builder
            .clone()
            .map(|rpc_builder| FollowRpcActor::new(rpc_builder, Arc::clone(&local)));

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
        rpc: Option<FollowRpcActor<LocalL2Client>>,
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

        let mut tasks = JoinSet::new();
        tasks.spawn(runtime.start());
        if let Some(rpc) = rpc {
            tasks.spawn(rpc.start(cancellation.clone()));
        }

        tokio::select! {
            result = tasks.join_next() => {
                cancellation.cancel();
                if let Some(result) = result {
                    result??;
                }
                while let Some(result) = tasks.join_next().await {
                    result??;
                }
            }
            _ = ShutdownSignal::wait() => {
                cancellation.cancel();
                while let Some(result) = tasks.join_next().await {
                    result??;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_rpc_client::RpcClient;
    use base_consensus_engine::InMemoryTransport;
    use jsonrpsee::RpcModule;

    use super::*;
    use crate::test_utils::FakeEngineClient;

    /// A supplied engine must not bypass follow-node admission or trigger source reads first.
    #[tokio::test]
    async fn refuses_genesis_before_follow_reads() {
        let config = Arc::new(RollupConfig::default());
        let engine_client = Arc::new(FakeEngineClient::new(Arc::clone(&config)));
        let handle = engine_client.handle();
        handle.set_l2_block_by_label(BlockNumberOrTag::Latest, Default::default());
        let transport = InMemoryTransport { module: RpcModule::new(()) };
        let node = FollowNode::new(FollowNodeConfig {
            rollup_config: config,
            engine_client,
            genesis_start: GenesisStartArgs::default(),
            local_l2_provider: RootProvider::new(RpcClient::new(transport.clone(), true)),
            l1_provider: RootProvider::new(RpcClient::new(transport, true)),
            l2_source: RemoteL2Client::new("http://127.0.0.1:1".parse().expect("valid unused source")),
            rpc_builder: None,
            proofs_enabled: false,
            proofs_max_blocks_ahead: 0,
            insert_delay: Duration::ZERO,
        });
        let error = node.start_with_cancellation(CancellationToken::new())
            .await.expect_err("a supplied engine must still refuse genesis");
        assert_eq!(error.to_string(),
            "refusing to start from genesis; restore a snapshot for an existing chain; use --allow-genesis-start only for launching a new chain",
            "follow startup must preserve the exact recovery instructions");
        assert_eq!(handle.call_count(), 1, "only the admission height query may run");
    }
}
