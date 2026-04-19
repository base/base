//! Implements the rollup client rpc endpoints. These endpoints serve data about the rollup state.
//!
//! Implemented in the op-node in <https://github.com/ethereum-optimism/optimism/blob/174e55f0a1e73b49b80a561fd3fedd4fea5770c6/op-service/sources/rollupclient.go#L16>

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
use base_consensus_engine::EngineState;
use base_consensus_genesis::RollupConfig;
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

        Metrics::rpc_calls("op_outputAtBlock").increment(1.0);

        let request_id = RPC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let (l1_sync_status_send, l1_sync_status_recv) = tokio::sync::oneshot::channel();
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
            async {
                self.l1_watcher_sender
                    .send(L1WatcherQueries::L1State(l1_sync_status_send))
                    .await
                    .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

                l1_sync_status_recv.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))
            }
            .instrument(span.clone())
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
        Metrics::rpc_calls("op_safeHeadAtL1Block").increment(1.0);

        let number = match block_num {
            BlockNumberOrTag::Number(n) => n,
            _ => {
                return Err(ErrorObject::owned(
                    -32602,
                    "optimism_safeHeadAtL1Block requires an explicit block number, not latest/earliest/pending",
                    None::<()>,
                ));
            }
        };

        self.safe_db_reader.safe_head_at_l1(number).await.map_err(|e| match e {
            SafeDBError::NotFound => ErrorObject::owned(-32000, "safe head not found", None::<()>),
            SafeDBError::Disabled => ErrorObject::owned(
                -32000,
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

        Metrics::rpc_calls("op_syncStatus").increment(1.0);

        let request_id = RPC_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let (l1_sync_status_send, l1_sync_status_recv) = tokio::sync::oneshot::channel();
        let request_started_at = Instant::now();
        let span = info_span!(
            target: "rpc",
            "rpc_request",
            request_id,
            rpc_method = RPC_METHOD,
        );

        info!(target: "rpc", request_id, rpc_method = RPC_METHOD, "Started rollup RPC request");

        let (l1_sync_status, l2_sync_status) = tokio::try_join!(
            async {
                self.l1_watcher_sender
                    .send(L1WatcherQueries::L1State(l1_sync_status_send))
                    .await
                    .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;
                l1_sync_status_recv.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))
            }
            .instrument(span.clone()),
            self.engine_client.get_state().instrument(span.clone())
        )
        .map_err(|error| {
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

        info!(
            target: "rpc",
            request_id,
            rpc_method = RPC_METHOD,
            elapsed_ms = request_started_at.elapsed().as_millis() as u64,
            "Completed rollup RPC request"
        );

        Ok(Self::sync_status_from_actor_queries(l1_sync_status, l2_sync_status))
    }

    async fn rollup_config(&self) -> RpcResult<RollupConfig> {
        Metrics::rpc_calls("op_rollupConfig").increment(1.0);

        self.engine_client.get_config().await
    }

    async fn version(&self) -> RpcResult<String> {
        Metrics::rpc_calls("op_version").increment(1.0);

        const RPC_VERSION: &str = env!("CARGO_PKG_VERSION");

        return Ok(RPC_VERSION.to_string());
    }
}
