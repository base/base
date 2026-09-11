//! Fault injection for execution command stages.
use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};

use base_common_types_payload::ExecutionCommand;
use futures::{Stream, StreamExt};
/// Skips a configured number of execution stages between forwarded stages.
#[derive(Debug)]
#[pin_project::pin_project]
pub struct EngineSkipHeads<S> {
    /// Underlying command stream.
    #[pin]
    pub stream: S,
    /// Number of stages to skip.
    pub threshold: usize,
    /// Stages skipped since the previous forwarded stage.
    pub skipped: usize,
}
impl<S> EngineSkipHeads<S> {
    /// Creates a stage fault injector.
    pub const fn new(stream: S, threshold: usize) -> Self {
        Self { stream, threshold, skipped: threshold }
    }
}
impl<S: Stream<Item = ExecutionCommand>> Stream for EngineSkipHeads<S> {
    type Item = ExecutionCommand;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            let Some(mut command) = ready!(this.stream.poll_next_unpin(cx)) else {
                return Poll::Ready(None);
            };
            {
                if *this.skipped < *this.threshold {
                    *this.skipped += 1;
                    if let ExecutionCommand::AppendPayload { skip_heads, .. } = &mut command {
                        *skip_heads = true;
                    }
                    match command {
                        ExecutionCommand::UpdateHeads { tx, .. }
                        | ExecutionCommand::StartBuilding { tx, .. } => {
                            let _ = tx
                                .send(Ok(base_common_types_payload::PendingHeadUpdate::syncing()));
                            continue;
                        }
                        command => return Poll::Ready(Some(command)),
                    }
                }
                *this.skipped = 0;
            }
            return Poll::Ready(Some(command));
        }
    }
}

#[cfg(test)]
mod tests {
    use base_common_types_chain::BaseBlock;
    use base_common_types_payload::{
        BaseExecutionPayload, BaseExecutionPayloadSidecar, ExecutionData, ExecutionPayloadV1,
        ForkchoiceState,
    };
    use tokio::sync::oneshot;

    use super::*;
    use crate::EngineSkipImport;

    #[tokio::test]
    async fn skipped_heads_respond_syncing_and_keep_import_stage_available() {
        let (first_tx, _) = oneshot::channel();
        let (skipped_tx, skipped_rx) = oneshot::channel();
        let (append_tx, _) = oneshot::channel();
        let commands = futures::stream::iter([
            ExecutionCommand::UpdateHeads { heads: ForkchoiceState::default(), tx: first_tx },
            ExecutionCommand::UpdateHeads { heads: ForkchoiceState::default(), tx: skipped_tx },
            ExecutionCommand::AppendPayload {
                payload: Box::new(ExecutionData {
                    payload: BaseExecutionPayload::V1(ExecutionPayloadV1::from_block_slow(
                        &BaseBlock::default(),
                    )),
                    sidecar: BaseExecutionPayloadSidecar::default(),
                    block_access_list: None,
                }),
                heads: ForkchoiceState::default(),
                skip_import: false,
                skip_heads: false,
                tx: append_tx,
            },
        ]);
        let mut stream = EngineSkipHeads::new(commands, 2);
        assert!(matches!(stream.next().await, Some(ExecutionCommand::UpdateHeads { .. })));
        assert!(matches!(
            stream.next().await,
            Some(ExecutionCommand::AppendPayload { skip_import: false, skip_heads: true, .. })
        ));
        assert!(skipped_rx.await.unwrap().unwrap().await.unwrap().payload_status.is_syncing());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn skipped_import_still_forwards_head_application() {
        let (tx, _) = oneshot::channel();
        let commands = futures::stream::iter([ExecutionCommand::AppendPayload {
            payload: Box::new(ExecutionData {
                payload: BaseExecutionPayload::V1(ExecutionPayloadV1::from_block_slow(
                    &BaseBlock::default(),
                )),
                sidecar: BaseExecutionPayloadSidecar::default(),
                block_access_list: None,
            }),
            heads: ForkchoiceState::default(),
            skip_import: false,
            skip_heads: false,
            tx,
        }]);
        let mut stream = EngineSkipImport::new(commands, 1);
        assert!(matches!(
            stream.next().await,
            Some(ExecutionCommand::AppendPayload { skip_import: true, skip_heads: false, .. })
        ));
    }
}
