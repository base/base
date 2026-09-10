//! Coordinates consensus requests, tree execution, and block downloads.

use std::{
    fmt::Display,
    task::{Context, Poll},
};

use alloy_primitives::{B256, map::B256Set};
use base_common_types_chain::SealedBlock;
use base_common_types_payload::{
    BeaconEngineMessage, BuiltPayloadExecutedBlock, ConsensusEngineEvent,
};
use base_execution_network_wire::BlockClient;
use crossbeam_channel::Sender;
use futures::{Stream, StreamExt};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    backfill::BackfillAction,
    chain::{FromOrchestrator, HandlerEvent},
    download::{BasicBlockDownloader, DownloadAction, DownloadOutcome},
};

/// Routes consensus requests to the execution tree and downloads missing blocks.
#[derive(Debug)]
pub struct EngineHandler<S, Client: BlockClient + 'static> {
    to_tree: Sender<FromEngine>,
    from_tree: UnboundedReceiver<EngineApiEvent>,
    incoming_requests: S,
    downloader: BasicBlockDownloader<Client>,
}

impl<S, Client: BlockClient + 'static> EngineHandler<S, Client> {
    /// Connects the execution tree, consensus stream, and downloader.
    pub const fn new(
        to_tree: Sender<FromEngine>,
        from_tree: UnboundedReceiver<EngineApiEvent>,
        downloader: BasicBlockDownloader<Client>,
        incoming_requests: S,
    ) -> Self {
        Self { to_tree, from_tree, incoming_requests, downloader }
    }

    /// Sends a request or synchronization event to the execution tree.
    pub fn on_event(&mut self, event: FromEngine) {
        let _ = self.to_tree.send(event);
    }
}

impl<S, Client: BlockClient + 'static> EngineHandler<S, Client>
where
    S: Stream<Item = BeaconEngineMessage> + Unpin,
{
    /// Advances tree events, consensus requests, and block downloads.
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<HandlerEvent> {
        loop {
            while let Poll::Ready(event) = self.from_tree.poll_recv(cx) {
                match event {
                    None => return Poll::Ready(HandlerEvent::FatalError),
                    Some(EngineApiEvent::BeaconConsensus(event)) => {
                        return Poll::Ready(HandlerEvent::Event(event));
                    }
                    Some(EngineApiEvent::BackfillAction(action)) => {
                        self.downloader.on_action(DownloadAction::Clear);
                        return Poll::Ready(HandlerEvent::BackfillAction(action));
                    }
                    Some(EngineApiEvent::Download(request)) => {
                        self.downloader.on_action(DownloadAction::Download(request));
                    }
                }
            }
            if let Poll::Ready(Some(request)) = self.incoming_requests.poll_next_unpin(cx) {
                self.on_event(FromEngine::Request(request.into()));
                continue;
            }
            if let Poll::Ready(outcome) = self.downloader.poll(cx) {
                if let DownloadOutcome::Blocks(blocks) = outcome {
                    self.on_event(FromEngine::DownloadedBlocks(blocks));
                }
                continue;
            }
            return Poll::Pending;
        }
    }
}

/// The type for specifying the kind of engine api.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineApiKind {
    /// The chain contains Ethereum configuration.
    #[default]
    Ethereum,
    /// The chain contains Optimism configuration.
    OpStack,
}

impl EngineApiKind {
    /// Returns true if this is the ethereum variant
    pub const fn is_ethereum(&self) -> bool {
        matches!(self, Self::Ethereum)
    }

    /// Returns true if this is the opstack variant
    pub const fn is_opstack(&self) -> bool {
        matches!(self, Self::OpStack)
    }
}

/// The request variants that the engine API handler can receive.
#[derive(Debug)]
pub enum EngineApiRequest {
    /// A request received from the consensus engine.
    Beacon(BeaconEngineMessage),
    /// Request to insert an already executed block, e.g. via payload building.
    InsertExecutedBlock(BuiltPayloadExecutedBlock),
}

impl Display for EngineApiRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Beacon(msg) => msg.fmt(f),
            Self::InsertExecutedBlock(payload) => {
                write!(f, "InsertExecutedBlock({:?})", payload.recovered_block.num_hash())
            }
        }
    }
}

impl From<BeaconEngineMessage> for EngineApiRequest {
    fn from(msg: BeaconEngineMessage) -> Self {
        Self::Beacon(msg)
    }
}

impl From<EngineApiRequest> for FromEngine {
    fn from(req: EngineApiRequest) -> Self {
        Self::Request(req)
    }
}

/// Events emitted by the engine API handler.
#[derive(Debug)]
pub enum EngineApiEvent {
    /// Event from the consensus engine.
    // TODO(mattsse): find a more appropriate name for this variant, consider phasing it out.
    BeaconConsensus(ConsensusEngineEvent),
    /// Backfill action is needed.
    BackfillAction(BackfillAction),
    /// Block download is needed.
    Download(DownloadRequest),
}

impl EngineApiEvent {
    /// Returns `true` if the event is a backfill action.
    pub const fn is_backfill_action(&self) -> bool {
        matches!(self, Self::BackfillAction(_))
    }
}

impl From<ConsensusEngineEvent> for EngineApiEvent {
    fn from(event: ConsensusEngineEvent) -> Self {
        Self::BeaconConsensus(event)
    }
}

/// Events received from the engine.
#[derive(Debug)]
pub enum FromEngine {
    /// Event from the top level orchestrator.
    Event(FromOrchestrator),
    /// Request from the engine.
    Request(EngineApiRequest),
    /// Downloaded blocks from the network.
    DownloadedBlocks(Vec<SealedBlock>),
}

impl Display for FromEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Event(ev) => write!(f, "Event({ev:?})"),
            Self::Request(req) => write!(f, "Request({req})"),
            Self::DownloadedBlocks(blocks) => {
                write!(f, "DownloadedBlocks({} blocks)", blocks.len())
            }
        }
    }
}

impl From<FromOrchestrator> for FromEngine {
    fn from(event: FromOrchestrator) -> Self {
        Self::Event(event)
    }
}

/// A request to download blocks from the network.
#[derive(Debug)]
pub enum DownloadRequest {
    /// Download the given set of blocks.
    BlockSet(B256Set),
    /// Download the given range of blocks.
    BlockRange(B256, u64),
}

impl DownloadRequest {
    /// Returns a [`DownloadRequest`] for a single block.
    pub fn single_block(hash: B256) -> Self {
        Self::BlockSet(B256Set::from_iter([hash]))
    }
}
