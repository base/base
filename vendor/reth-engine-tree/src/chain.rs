use std::{
    fmt::{Display, Formatter, Result},
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use reth_engine_primitives::{BeaconEngineMessage, ConsensusEngineEvent};

use crate::{download::BlockDownloader, engine::EngineHandler};
use reth_stages_api::{ControlFlow, PipelineTarget};
use tracing::*;

use crate::backfill::{BackfillAction, BackfillEvent, PipelineSync};

/// The type that drives the chain forward.
///
/// A state machine that orchestrates the components responsible for advancing the chain
///
///
/// ## Control flow
///
/// The [`ChainOrchestrator`] is responsible for controlling the backfill sync and additional hooks.
/// It polls the given `handler`, which is responsible for advancing the chain, how is up to the
/// handler. However, due to database restrictions (e.g. exclusive write access), following
/// invariants apply:
///  - If the handler requests a backfill run (e.g. [`BackfillAction::Start`]), the handler must
///    ensure that while the backfill sync is running, no other write access is granted.
///  - At any time the [`ChainOrchestrator`] can request exclusive write access to the database
///    (e.g. if pruning is required), but will not do so until the handler has acknowledged the
///    request for write access.
///
/// The [`ChainOrchestrator`] polls the [`EngineHandler`] to advance the chain and handles the
/// emitted events. Requests and events are passed to the [`EngineHandler`] via
/// [`EngineHandler::on_event`].
#[must_use = "Stream does nothing unless polled"]
#[derive(Debug)]
pub struct ChainOrchestrator<S, D>
{
    /// The handler for advancing the chain.
    handler: EngineHandler<S, D>,
    /// Controls backfill sync.
    backfill_sync: PipelineSync,
}

impl<S, D> ChainOrchestrator<S, D>
where
    S: Stream<Item = BeaconEngineMessage> + Unpin,
    D: BlockDownloader + Unpin,
{
    /// Creates a new [`ChainOrchestrator`] with the given handler and backfill sync.
    pub const fn new(handler: EngineHandler<S, D>, backfill_sync: PipelineSync) -> Self {
        Self { handler, backfill_sync }
    }

    /// Returns the handler
    pub const fn handler(&self) -> &EngineHandler<S, D> {
        &self.handler
    }

    /// Returns a mutable reference to the handler
    pub const fn handler_mut(&mut self) -> &mut EngineHandler<S, D> {
        &mut self.handler
    }

    /// Triggers a backfill sync for the __valid__ given target.
    ///
    /// CAUTION: This function should be used with care and with a valid target.
    pub fn start_backfill_sync(&mut self, target: impl Into<PipelineTarget>) {
        self.backfill_sync.on_action(BackfillAction::Start(target.into()));
    }

    /// Internal function used to advance the chain.
    ///
    /// Polls the `ChainOrchestrator` for the next event.
    #[tracing::instrument(level = "debug", target = "engine::tree::chain_orchestrator", skip_all)]
    fn poll_next_event(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<ChainEvent> {
        let this = self.get_mut();

        // This loop polls the components
        //
        // 1. Polls the backfill sync to completion, if active.
        // 2. Advances the chain by polling the handler.
        'outer: loop {
            // try to poll the backfill sync to completion, if active
            match this.backfill_sync.poll(cx) {
                Poll::Ready(backfill_sync_event) => match backfill_sync_event {
                    BackfillEvent::Started(_) => {
                        // notify handler that backfill sync started
                        this.handler.on_event(FromOrchestrator::BackfillSyncStarted.into());
                        return Poll::Ready(ChainEvent::BackfillSyncStarted);
                    }
                    BackfillEvent::Finished(res) => {
                        return match res {
                            Ok(ctrl) => {
                                tracing::debug!(?ctrl, "backfill sync finished");
                                // notify handler that backfill sync finished
                                this.handler.on_event(FromOrchestrator::BackfillSyncFinished(ctrl).into());
                                Poll::Ready(ChainEvent::BackfillSyncFinished)
                            }
                            Err(err) => {
                                tracing::error!( %err, "backfill sync failed");
                                Poll::Ready(ChainEvent::FatalError)
                            }
                        };
                    }
                    BackfillEvent::TaskDropped(err) => {
                        tracing::error!( %err, "backfill sync task dropped");
                        return Poll::Ready(ChainEvent::FatalError);
                    }
                },
                Poll::Pending => {}
            }

            // poll the handler for the next event
            match this.handler.poll(cx) {
                Poll::Ready(handler_event) => {
                    match handler_event {
                        HandlerEvent::BackfillAction(action) => {
                            // forward action to backfill_sync
                            this.backfill_sync.on_action(action);
                        }
                        HandlerEvent::Event(ev) => {
                            // bubble up the event
                            return Poll::Ready(ChainEvent::Handler(ev));
                        }
                        HandlerEvent::FatalError => {
                            error!(target: "engine::tree", "Fatal error");
                            return Poll::Ready(ChainEvent::FatalError);
                        }
                    }
                }
                Poll::Pending => {
                    // no more events to process
                    break 'outer;
                }
            }
        }

        Poll::Pending
    }
}

impl<S, D> Stream for ChainOrchestrator<S, D>
where
    S: Stream<Item = BeaconEngineMessage> + Unpin,
    D: BlockDownloader + Unpin,
{
    type Item = ChainEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.as_mut().poll_next_event(cx).map(Some)
    }
}

/// Event emitted by the [`ChainOrchestrator`]
///
/// These are meant to be used for observability and debugging purposes.
#[derive(Debug)]
pub enum ChainEvent {
    /// Backfill sync started
    BackfillSyncStarted,
    /// Backfill sync finished
    BackfillSyncFinished,
    /// Fatal error
    FatalError,
    /// Event emitted by the handler
    Handler(ConsensusEngineEvent),
}

impl Display for ChainEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::BackfillSyncStarted => {
                write!(f, "BackfillSyncStarted")
            }
            Self::BackfillSyncFinished => {
                write!(f, "BackfillSyncFinished")
            }
            Self::FatalError => {
                write!(f, "FatalError")
            }
            Self::Handler(event) => {
                write!(f, "Handler({event})")
            }
        }
    }
}

/// Events/Requests that the [`EngineHandler`] can emit to the [`ChainOrchestrator`].
#[derive(Clone, Debug)]
pub enum HandlerEvent {
    /// Request an action to backfill sync
    BackfillAction(BackfillAction),
    /// Other event emitted by the handler
    Event(ConsensusEngineEvent),
    /// Fatal error
    FatalError,
}

/// Internal events issued by the [`ChainOrchestrator`].
#[derive(Debug)]
pub enum FromOrchestrator {
    /// Invoked when backfill sync finished
    BackfillSyncFinished(ControlFlow),
    /// Invoked when backfill sync started
    BackfillSyncStarted,
    /// Gracefully terminate the engine service.
    ///
    /// When this variant is received, the engine will persist all remaining in-memory blocks
    /// to disk before shutting down. Once persistence is complete, a signal is sent through
    /// the oneshot channel to notify the caller.
    Terminate {
        /// Channel to signal termination completion.
        tx: tokio::sync::oneshot::Sender<()>,
    },
}
