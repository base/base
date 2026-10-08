//! Implements the rollup client rpc endpoints. These endpoints serve data about the rollup state.
//!
//! The method names remain compatible with the legacy rollup RPC namespace.

use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use alloy_eips::BlockNumberOrTag;
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_consensus_engine::EngineState;
use base_consensus_gossip::Metrics;
use base_consensus_safedb::{SafeDBError, SafeDBReader, SafeHeadResponse};
use base_protocol::SyncStatus;
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObject},
};
use tracing::Instrument;

use crate::{
    EngineRpcClient, L1State, L1WatcherQueries, OutputResponse, RollupNodeApiServer,
    l1_watcher::L1WatcherQuerySender,
};

static RPC_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// JSON-RPC server error code for safe head lookups the node cannot serve.
const SAFE_HEAD_UNAVAILABLE_CODE: i32 = -32000;

/// `RollupRpc`
///
/// This is a server implementation of [`crate::RollupNodeApiServer`].
#[derive(Debug)]
pub struct RollupRpc<EngineRpcClient_> {
    /// The channel to send [`base_consensus_engine::EngineQueries`]s.
    pub engine_client: EngineRpcClient_,
    /// The channel to send [`crate::L1WatcherQueries`]s.
    pub l1_watcher_sender: L1WatcherQuerySender,
    /// Reader for safe head lookups by L1 block number.
    pub safe_db_reader: Arc<dyn SafeDBReader>,
}

impl<EngineRpcClient_: EngineRpcClient> RollupRpc<EngineRpcClient_> {
    /// Constructs a new [`RollupRpc`] given a sender channel.
    pub fn new(
        engine_client: EngineRpcClient_,
        l1_watcher_sender: L1WatcherQuerySender,
        safe_db_reader: Arc<dyn SafeDBReader>,
    ) -> Self {
        Self { engine_client, l1_watcher_sender, safe_db_reader }
    }

    /// Queries the L1 watcher for its current [`L1State`].
    async fn l1_state(&self) -> RpcResult<L1State> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.l1_watcher_sender
            .send(L1WatcherQueries::L1State(tx))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;
        rx.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    // Important note: we zero-out the fields that can't be derived yet to follow the reference node's
    // behaviour.
    fn sync_status_from_actor_queries(
        l1_sync_status: L1State,
        l2_sync_status: EngineState,
    ) -> SyncStatus {
        SyncStatus {
            current_l1: l1_sync_status.current_l1.unwrap_or_default(),
            current_l1_finalized: l1_sync_status.current_l1_finalized.unwrap_or_default(),
            head_l1: l1_sync_status.head_l1.unwrap_or_default(),
            safe_l1: l1_sync_status.safe_l1.unwrap_or_default(),
            finalized_l1: l1_sync_status.finalized_l1.unwrap_or_default(),
            unsafe_l2: l2_sync_status.sync_state.unsafe_head(),
            local_safe_l2: l2_sync_status.sync_state.local_safe_head(),
            safe_l2: l2_sync_status.sync_state.safe_head(),
            finalized_l2: l2_sync_status.sync_state.finalized_head(),
        }
    }
}

#[async_trait]
impl<EngineRpcClient_: EngineRpcClient + 'static> RollupNodeApiServer
    for RollupRpc<EngineRpcClient_>
{
    async fn output_at_block(&self, block_num: BlockNumberOrTag) -> RpcResult<OutputResponse> {
        const RPC_METHOD: &str = "optimism_outputAtBlock";

        Metrics::rpc_calls("base_outputAtBlock").increment(1.0);

        let request_id = RPC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let request_started_at = Instant::now();
        let span = info_span!(
            target: "rpc",
            "rpc_request",
            request_id,
            rpc_method = RPC_METHOD,
            block = ?block_num,
        );

        info!(target: "rpc", request_id, rpc_method = RPC_METHOD, block = ?block_num, "Started rollup RPC request");

        let ((l2_block_info, output_root, l2_sync_status), l1_sync_status) = tokio::try_join!(
            self.engine_client.output_at_block(block_num).instrument(span.clone()),
            self.l1_state().instrument(span.clone())
        )
        .map_err(|error| {
            warn!(
                target: "rpc",
                request_id,
                rpc_method = RPC_METHOD,
                block = ?block_num,
                elapsed_ms = request_started_at.elapsed().as_millis() as u64,
                error = ?error,
                "Rollup RPC request failed"
            );
            error
        })?;

        let sync_status = Self::sync_status_from_actor_queries(l1_sync_status, l2_sync_status);

        info!(
            target: "rpc",
            request_id,
            rpc_method = RPC_METHOD,
            block = ?block_num,
            elapsed_ms = request_started_at.elapsed().as_millis() as u64,
            "Completed rollup RPC request"
        );

        Ok(OutputResponse::from_v0(output_root, sync_status, l2_block_info))
    }

    async fn safe_head_at_l1_block(
        &self,
        block_num: BlockNumberOrTag,
    ) -> RpcResult<SafeHeadResponse> {
        Metrics::rpc_calls("base_safeHeadAtL1Block").increment(1.0);

        let number = match block_num {
            BlockNumberOrTag::Number(n) => n,
            _ => {
                return Err(ErrorObject::owned(
                    ErrorCode::InvalidParams.code(),
                    "optimism_safeHeadAtL1Block requires an explicit block number, not latest/earliest/pending",
                    None::<()>,
                ));
            }
        };

        self.safe_db_reader.safe_head_at_l1(number).await.map_err(|e| match e {
            SafeDBError::NotFound => {
                ErrorObject::owned(SAFE_HEAD_UNAVAILABLE_CODE, "safe head not found", None::<()>)
            }
            SafeDBError::Disabled => ErrorObject::owned(
                SAFE_HEAD_UNAVAILABLE_CODE,
                "safe head tracking is disabled on this node",
                None::<()>,
            ),
            SafeDBError::Database(_) => {
                error!(target: "rpc", error = %e, "safedb query failed");
                ErrorObject::from(ErrorCode::InternalError)
            }
        })
    }

    async fn sync_status(&self) -> RpcResult<SyncStatus> {
        const RPC_METHOD: &str = "optimism_syncStatus";

        Metrics::rpc_calls("base_syncStatus").increment(1.0);

        let request_id = RPC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let request_started_at = Instant::now();
        let span = info_span!(
            target: "rpc",
            "rpc_request",
            request_id,
            rpc_method = RPC_METHOD,
        );

        debug!(target: "rpc", request_id, rpc_method = RPC_METHOD, "Started rollup RPC request");

        // Read the L1 state first: derivation publishes `current_l1` only once the engine safe head
        // covers every L2 block derived before it, so safe heads read afterwards never lag
        // `current_l1`.
        let sync_statuses = async {
            let l1_sync_status = self.l1_state().await?;
            let l2_sync_status = self.engine_client.get_state().await?;
            RpcResult::Ok((l1_sync_status, l2_sync_status))
        };
        let (l1_sync_status, l2_sync_status) =
            sync_statuses.instrument(span).await.map_err(|error| {
                warn!(
                    target: "rpc",
                    request_id,
                    rpc_method = RPC_METHOD,
                    elapsed_ms = request_started_at.elapsed().as_millis() as u64,
                    error = ?error,
                    "Rollup RPC request failed"
                );
                ErrorObject::from(ErrorCode::InternalError)
            })?;

        debug!(
            target: "rpc",
            request_id,
            rpc_method = RPC_METHOD,
            elapsed_ms = request_started_at.elapsed().as_millis() as u64,
            "Completed rollup RPC request"
        );

        Ok(Self::sync_status_from_actor_queries(l1_sync_status, l2_sync_status))
    }

    async fn rollup_config(&self) -> RpcResult<RollupConfig> {
        Metrics::rpc_calls("base_rollupConfig").increment(1.0);

        self.engine_client.get_config().await
    }

    async fn version(&self) -> RpcResult<String> {
        Metrics::rpc_calls("base_version").increment(1.0);

        Ok(env!("CARGO_PKG_VERSION").to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use base_consensus_safedb::DisabledSafeDB;
    use base_protocol::{L2BlockInfo, OutputRoot};
    use tokio::sync::{mpsc, watch};

    use super::*;

    // `automock` cannot implement the `Clone` supertrait `EngineRpcClient` requires.
    mockall::mock! {
        #[derive(Debug)]
        Engine {}

        impl Clone for Engine {
            fn clone(&self) -> Self;
        }

        #[async_trait]
        impl EngineRpcClient for Engine {
            async fn get_config(&self) -> RpcResult<RollupConfig>;
            async fn get_state(&self) -> RpcResult<EngineState>;
            async fn output_at_block(
                &self,
                block: BlockNumberOrTag,
            ) -> RpcResult<(L2BlockInfo, OutputRoot, EngineState)>;
            async fn dev_get_task_queue_length(&self) -> RpcResult<usize>;
            async fn dev_subscribe_to_engine_queue_length(&self) -> RpcResult<watch::Receiver<usize>>;
            async fn dev_subscribe_to_engine_state(&self) -> RpcResult<watch::Receiver<EngineState>>;
        }
    }

    /// `optimism_syncStatus` reads the engine state only once the L1 watcher has answered, so
    /// the safe heads it reports include every block derived from the L1 blocks before
    /// `current_l1`.
    #[tokio::test]
    async fn sync_status_reads_the_engine_state_after_the_l1_state() {
        let l1_state_answered = Arc::new(AtomicBool::new(false));

        let mut engine = MockEngine::new();
        let answered = Arc::clone(&l1_state_answered);
        engine.expect_get_state().returning(move || {
            assert!(
                answered.load(Ordering::SeqCst),
                "the engine state was read before the L1 state"
            );
            Ok(EngineState::default())
        });

        let (l1_watcher_sender, mut l1_watcher) = mpsc::channel(1);
        tokio::spawn(async move {
            let Some(L1WatcherQueries::L1State(reply)) = l1_watcher.recv().await else {
                panic!("expected an L1 state query");
            };
            l1_state_answered.store(true, Ordering::SeqCst);
            reply
                .send(L1State {
                    current_l1: None,
                    current_l1_finalized: None,
                    head_l1: None,
                    safe_l1: None,
                    finalized_l1: None,
                })
                .unwrap();
        });

        let rpc = RollupRpc::new(engine, l1_watcher_sender, Arc::new(DisabledSafeDB));
        rpc.sync_status().await.unwrap();
    }
}
