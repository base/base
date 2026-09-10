//! Checkpoint actor.

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{CheckpointDB, CheckpointError, CheckpointRequest};
use crate::NodeActor;

/// Actor that owns durable checkpoint storage.
#[derive(Debug)]
pub struct CheckpointActor {
    db: CheckpointDB,
    request_rx: mpsc::Receiver<CheckpointRequest>,
}

impl CheckpointActor {
    /// Creates a new checkpoint actor.
    pub const fn new(db: CheckpointDB, request_rx: mpsc::Receiver<CheckpointRequest>) -> Self {
        Self { db, request_rx }
    }
}

#[async_trait]
impl NodeActor for CheckpointActor {
    type Error = CheckpointError;
    type StartData = CancellationToken;

    async fn start(mut self, cancellation: Self::StartData) -> Result<(), Self::Error> {
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    info!(target: "checkpoint", "checkpoint actor received shutdown signal");
                    return Ok(());
                }
                request = self.request_rx.recv() => {
                    let Some(request) = request else {
                        warn!(target: "checkpoint", "checkpoint request channel closed");
                        return Ok(());
                    };

                    match request {
                        CheckpointRequest::Read { label, response_tx } => {
                            let result = self.db.checkpoint(label).await;
                            if response_tx.send(result).is_err() {
                                debug!(target: "checkpoint", label = label.as_str(), "checkpoint read response receiver dropped");
                            }
                        }
                        CheckpointRequest::Write { label, block, response_tx } => {
                            let result = self.db.update(label, block).await;
                            if response_tx.send(result).is_err() {
                                debug!(target: "checkpoint", label = label.as_str(), "checkpoint write response receiver dropped");
                            }
                        }
                    }
                }
            }
        }
    }
}
