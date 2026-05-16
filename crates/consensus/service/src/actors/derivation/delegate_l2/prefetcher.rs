use std::sync::Arc;

use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use tokio::{select, sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::actors::derivation::{
    DerivationError,
    delegate_l2::{L2SourceClient, client::DelegateL2ClientError},
};

pub(super) struct L2PayloadPrefetcher<L2Source> {
    l2_source: Arc<L2Source>,
    cancellation_token: CancellationToken,
    start_block: u64,
    target_block: u64,
    depth: usize,
}

pub(super) struct L2PayloadPrefetch {
    payload_rx: mpsc::Receiver<(u64, Result<BaseExecutionPayloadEnvelope, DelegateL2ClientError>)>,
    handle: Option<JoinHandle<()>>,
}

impl<L2Source> L2PayloadPrefetcher<L2Source>
where
    L2Source: L2SourceClient + 'static,
{
    pub(super) const fn new(
        l2_source: Arc<L2Source>,
        cancellation_token: CancellationToken,
        start_block: u64,
        target_block: u64,
        depth: usize,
    ) -> Self {
        Self { l2_source, cancellation_token, start_block, target_block, depth }
    }

    pub(super) fn spawn(self) -> L2PayloadPrefetch {
        let (payload_tx, payload_rx) = mpsc::channel(self.depth.max(1));
        let handle = tokio::spawn(Self::prefetch_payloads(
            self.l2_source,
            self.cancellation_token,
            self.start_block,
            self.target_block,
            payload_tx,
        ));

        L2PayloadPrefetch { payload_rx, handle: Some(handle) }
    }

    async fn prefetch_payloads(
        l2_source: Arc<L2Source>,
        cancellation_token: CancellationToken,
        start_block: u64,
        target_block: u64,
        payload_tx: mpsc::Sender<(
            u64,
            Result<BaseExecutionPayloadEnvelope, DelegateL2ClientError>,
        )>,
    ) {
        for block_num in start_block..=target_block {
            if cancellation_token.is_cancelled() {
                debug!(
                    target: "derivation",
                    block = block_num,
                    "Stopping L2 source prefetch after cancellation"
                );
                return;
            }

            let result = l2_source.get_payload_by_number(block_num).await;
            let should_stop = result.is_err();
            if payload_tx.send((block_num, result)).await.is_err() {
                debug!(
                    target: "derivation",
                    block = block_num,
                    "Stopping L2 source prefetch after receiver dropped"
                );
                return;
            }

            if should_stop {
                return;
            }
        }
    }
}

impl L2PayloadPrefetch {
    pub(super) async fn next_payload(
        &mut self,
        expected_block: u64,
        cancellation_token: &CancellationToken,
    ) -> Result<Option<BaseExecutionPayloadEnvelope>, DerivationError> {
        let Some((prefetched_block_num, payload)) = (select! {
            biased;

            _ = cancellation_token.cancelled() => {
                info!(target: "derivation", block = expected_block, "Sync interrupted by shutdown");
                return Ok(None);
            }
            prefetched = self.payload_rx.recv() => prefetched
        }) else {
            return Err(DerivationError::Sender(Box::new(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "L2 source prefetch channel closed",
            ))));
        };

        if prefetched_block_num != expected_block {
            return Err(DerivationError::Sender(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "L2 source prefetch returned unexpected block: expected {expected_block}, got {prefetched_block_num}",
                ),
            ))));
        }

        payload.map(Some).map_err(|e| DerivationError::Sender(Box::new(e)))
    }

    pub(super) async fn finish(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for L2PayloadPrefetch {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle
            && !handle.is_finished()
        {
            handle.abort();
        }
    }
}
