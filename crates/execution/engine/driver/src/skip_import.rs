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
pub struct EngineSkipImport<S> {
    /// Underlying command stream.
    #[pin]
    pub stream: S,
    /// Number of stages to skip.
    pub threshold: usize,
    /// Stages skipped since the previous forwarded stage.
    pub skipped: usize,
}
impl<S> EngineSkipImport<S> {
    /// Creates a stage fault injector.
    pub const fn new(stream: S, threshold: usize) -> Self {
        Self { stream, threshold, skipped: 0 }
    }
}
impl<S: Stream<Item = ExecutionCommand>> Stream for EngineSkipImport<S> {
    type Item = ExecutionCommand;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        let Some(mut command) = ready!(this.stream.poll_next_unpin(cx)) else {
            return Poll::Ready(None);
        };
        if let ExecutionCommand::AppendPayload { skip_import, .. } = &mut command {
            if *this.skipped < *this.threshold {
                *this.skipped += 1;
                *skip_import = true;
            } else {
                *this.skipped = 0;
            }
        }
        Poll::Ready(Some(command))
    }
}
