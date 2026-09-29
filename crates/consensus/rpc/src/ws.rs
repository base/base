//! Custom RPC subscription endpoints for the base node to stream internal state/data.

use base_consensus_engine::EngineState;
use base_protocol::L2BlockInfo;
use jsonrpsee::{
    PendingSubscriptionSink, SubscriptionSink,
    core::{SubscriptionError, SubscriptionResult, to_json_raw_value},
};

use crate::{EngineRpcClient, jsonrpsee::WsServer};

/// An RPC server that handles subscriptions to the node's state.
#[derive(Debug)]
pub struct WsRPC<EngineRpcClient_> {
    /// The engine query sender.
    engine_client: EngineRpcClient_,
}

impl<EngineRpcClient_: EngineRpcClient> WsRPC<EngineRpcClient_> {
    /// Constructs a new [`WsRPC`] instance.
    pub const fn new(engine_client: EngineRpcClient_) -> Self {
        Self { engine_client }
    }

    async fn send_state_update(
        sink: &SubscriptionSink,
        state: L2BlockInfo,
    ) -> Result<(), SubscriptionError> {
        sink.send(to_json_raw_value(&state).map_err(|_| {
            SubscriptionError::from("Internal error. Impossible to convert l2 block info to json")
        })?)
        .await
        .map_err(|_| {
            SubscriptionError::from("Failed to send head update. Subscription likely dropped.")
        })
    }

    /// Streams every change of the head selected by `head` from the engine state to `sink`.
    async fn stream_head_updates(
        &self,
        sink: PendingSubscriptionSink,
        head_kind: &'static str,
        head: fn(&EngineState) -> L2BlockInfo,
    ) -> SubscriptionResult {
        let sink = sink.accept().await?;

        let mut subscription =
            self.engine_client.dev_subscribe_to_engine_state().await.map_err(|_| {
                SubscriptionError::from(
                    "Internal error. Failed to subscribe to engine state updates. The engine query handler is likely closed.",
                )
            })?;

        let mut current_head = head(&subscription.borrow());

        while let Ok(new_head) = subscription
            .wait_for(|state| head(state) != current_head)
            .await
            .map(|state| head(&state))
        {
            if head_kind == "safe" {
                info!(target: "rpc::ws", safe_head = ?new_head, "Sending safe head update");
            } else {
                debug!(target: "rpc::ws", head_kind, head = ?new_head, "Sending head update");
            }
            current_head = new_head;
            Self::send_state_update(&sink, current_head).await?;
        }

        warn!(target: "rpc::ws", head_kind, "Head update subscription closed");
        Ok(())
    }
}

#[async_trait::async_trait]
impl<EngineRpcClient_: EngineRpcClient + 'static> WsServer for WsRPC<EngineRpcClient_> {
    async fn ws_safe_head_updates(&self, sink: PendingSubscriptionSink) -> SubscriptionResult {
        self.stream_head_updates(sink, "safe", |state| state.sync_state.safe_head()).await
    }

    async fn ws_finalized_head_updates(&self, sink: PendingSubscriptionSink) -> SubscriptionResult {
        self.stream_head_updates(sink, "finalized", |state| state.sync_state.finalized_head()).await
    }

    async fn ws_unsafe_head_updates(&self, sink: PendingSubscriptionSink) -> SubscriptionResult {
        self.stream_head_updates(sink, "unsafe", |state| state.sync_state.unsafe_head()).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_eips::BlockNumberOrTag;
    use async_trait::async_trait;
    use base_common_genesis::RollupConfig;
    use base_consensus_engine::{EngineState, EngineSyncStateUpdate};
    use base_protocol::{BlockInfo, L2BlockInfo, OutputRoot};
    use jsonrpsee::core::{EmptyServerParams, RpcResult};
    use rstest::rstest;
    use tokio::sync::watch;

    use super::WsRPC;
    use crate::{EngineRpcClient, WsServer};

    /// Serves engine state subscriptions from a test-owned watch channel. A hand-rolled stub
    /// because `EngineRpcClient` requires `Clone`, which `automock` cannot provide.
    #[derive(Debug, Clone)]
    struct StubEngineClient(watch::Receiver<EngineState>);

    #[async_trait]
    impl EngineRpcClient for StubEngineClient {
        async fn get_config(&self) -> RpcResult<RollupConfig> {
            unimplemented!()
        }

        async fn get_state(&self) -> RpcResult<EngineState> {
            unimplemented!()
        }

        async fn output_at_block(
            &self,
            _: BlockNumberOrTag,
        ) -> RpcResult<(L2BlockInfo, OutputRoot, EngineState)> {
            unimplemented!()
        }

        async fn dev_get_task_queue_length(&self) -> RpcResult<usize> {
            unimplemented!()
        }

        async fn dev_subscribe_to_engine_queue_length(&self) -> RpcResult<watch::Receiver<usize>> {
            unimplemented!()
        }

        async fn dev_subscribe_to_engine_state(&self) -> RpcResult<watch::Receiver<EngineState>> {
            Ok(self.0.clone())
        }
    }

    fn block(number: u64) -> L2BlockInfo {
        L2BlockInfo { block_info: BlockInfo { number, ..Default::default() }, ..Default::default() }
    }

    fn unsafe_update(head: L2BlockInfo) -> EngineSyncStateUpdate {
        EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() }
    }

    fn safe_update(head: L2BlockInfo) -> EngineSyncStateUpdate {
        EngineSyncStateUpdate { safe_head: Some(head), ..Default::default() }
    }

    fn finalized_update(head: L2BlockInfo) -> EngineSyncStateUpdate {
        EngineSyncStateUpdate { finalized_head: Some(head), ..Default::default() }
    }

    #[rstest]
    #[case::safe("ws_subscribe_safe_head", safe_update, unsafe_update)]
    #[case::finalized("ws_subscribe_finalized_head", finalized_update, safe_update)]
    #[case::unsafe_head("ws_subscribe_unsafe_head", unsafe_update, finalized_update)]
    #[tokio::test]
    async fn streams_only_changes_of_the_subscribed_head(
        #[case] method: &str,
        #[case] subscribed: fn(L2BlockInfo) -> EngineSyncStateUpdate,
        #[case] other: fn(L2BlockInfo) -> EngineSyncStateUpdate,
    ) {
        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let module = WsRPC::new(StubEngineClient(state_rx)).into_rpc();
        let mut subscription =
            module.subscribe_unbounded(method, EmptyServerParams::new()).await.unwrap();

        state_tx.send_modify(|state| state.sync_state = state.sync_state.updated(other(block(1))));
        state_tx
            .send_modify(|state| state.sync_state = state.sync_state.updated(subscribed(block(2))));

        let (head, _) =
            tokio::time::timeout(Duration::from_secs(5), subscription.next::<L2BlockInfo>())
                .await
                .expect("head update should be streamed")
                .unwrap()
                .unwrap();
        assert_eq!(head, block(2));
    }
}
