//! Capability messaging
//!
//! An `RLPx` stream is multiplexed via the prepended message-id of a framed message.
//! Capabilities are exchanged via the `RLPx` `Hello` message as pairs of `(id, version)`, <https://github.com/ethereum/devp2p/blob/master/rlpx.md#capability-messaging>

use std::{
    sync::Arc,
    task::{Context, Poll, ready},
};

use alloy_primitives::{B256, Bytes};
use base_common_types_chain::{
    BaseBlock, BaseReceipt, BaseTxEnvelope, BlockHeader, EthereumTxEnvelope, ReceiptWithBloom,
    TxEip4844,
};
use base_execution_network_wire::BlockBodies;
use base_execution_network_wire::BlockHeaders;
use base_execution_network_wire::BlockRangeUpdate;
use base_execution_network_wire::BroadcastPoolTransactions;
use base_execution_network_wire::Cells;
use base_execution_network_wire::EthMessage;
use base_execution_network_wire::GetBlockAccessLists;
use base_execution_network_wire::GetBlockBodies;
use base_execution_network_wire::GetBlockHeaders;
use base_execution_network_wire::GetReceipts;
use base_execution_network_wire::NewBlock;
use base_execution_network_wire::NewBlockHashes;
use base_execution_network_wire::NewBlockPayload;
use base_execution_network_wire::NewPooledTransactionHashes;
use base_execution_network_wire::NodeData;
use base_execution_network_wire::PooledTransactions;
use base_execution_network_wire::RawCapabilityMessage;
use base_execution_network_wire::Receipts;
use base_execution_network_wire::RequestPair;
use base_execution_network_wire::SharedTransactions;
use base_execution_network_wire::SnapProtocolMessage;
use base_execution_network_wire::Transactions;
use base_execution_network_wire::{RequestError, RequestResult, SnapResponse};
use futures::FutureExt;
use reth_network_api::{PeerRequest, RequestMessage};
use reth_primitives_traits::Block;
use tokio::sync::oneshot;

use crate::types::{BlockAccessLists, Receipts69, Receipts70};

/// Internal form of a `NewBlock` message
#[derive(Debug, Clone)]
pub struct NewBlockMessage<
    P = NewBlock<base_common_types_chain::Block<EthereumTxEnvelope<TxEip4844>>>,
> {
    /// Hash of the block
    pub hash: B256,
    /// Raw received message
    pub block: Arc<P>,
}

// === impl NewBlockMessage ===

impl<P: NewBlockPayload> NewBlockMessage<P> {
    /// Returns the block number of the block
    pub fn number(&self) -> u64 {
        self.block.block().header().number()
    }
}

/// All Bi-directional eth-message variants that can be sent to a session or received from a
/// session.
#[derive(Debug)]
pub enum PeerMessage {
    /// Announce new block hashes
    NewBlockHashes(NewBlockHashes),
    /// Broadcast new block.
    NewBlock(NewBlockMessage<base_execution_network_wire::NewBlock<BaseBlock>>),
    /// Received transactions _from_ the peer
    ReceivedTransaction(Transactions<BaseTxEnvelope>),
    /// Broadcast transactions _from_ local _to_ a peer.
    SendTransactions(SharedTransactions<BaseTxEnvelope>),
    /// Broadcast cached pool transactions _from_ local _to_ a peer.
    SendBroadcastPoolTransactions(BroadcastPoolTransactions),
    /// Send new pooled transactions
    PooledTransactions(NewPooledTransactionHashes),
    /// All `eth` request variants.
    EthRequest(PeerRequest),
    /// Announces when `BlockRange` is updated.
    BlockRangeUpdated(BlockRangeUpdate),
    /// Any other or manually crafted eth message.
    ///
    /// Caution: It is expected that this is a valid `eth_` capability message.
    Other(RawCapabilityMessage),
}

impl PeerMessage {
    /// Returns a static string identifying the message variant for logging.
    pub const fn message_kind(&self) -> &'static str {
        match self {
            Self::NewBlockHashes(_) => "NewBlockHashes",
            Self::NewBlock(_) => "NewBlock",
            Self::ReceivedTransaction(_) => "ReceivedTransaction",
            Self::SendTransactions(_) => "SendTransactions",
            Self::SendBroadcastPoolTransactions(_) => "SendBroadcastPoolTransactions",
            Self::PooledTransactions(_) => "PooledTransactions",
            Self::EthRequest(_) => "EthRequest",
            Self::BlockRangeUpdated(_) => "BlockRangeUpdated",
            Self::Other(_) => "Other",
        }
    }

    /// Returns `true` if this message is a broadcast (block/transaction announcement or
    /// propagation) rather than a request/response.
    pub const fn is_broadcast(&self) -> bool {
        matches!(
            self,
            Self::NewBlockHashes(_)
                | Self::NewBlock(_)
                | Self::SendTransactions(_)
                | Self::SendBroadcastPoolTransactions(_)
                | Self::PooledTransactions(_)
        )
    }

    /// Returns the number of items in the message payload, if applicable.
    pub fn message_item_count(&self) -> usize {
        match self {
            Self::NewBlockHashes(msg) => msg.len(),
            Self::ReceivedTransaction(msg) => msg.len(),
            Self::SendTransactions(msg) => msg.len(),
            Self::SendBroadcastPoolTransactions(msg) => msg.len(),
            Self::PooledTransactions(msg) => msg.len(),
            Self::NewBlock(_)
            | Self::EthRequest(_)
            | Self::BlockRangeUpdated(_)
            | Self::Other(_) => 1,
        }
    }
}

/// Request Variants that only target block related data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockRequest {
    /// Requests block headers from the peer.
    ///
    /// The response should be sent through the channel.
    GetBlockHeaders(GetBlockHeaders),

    /// Requests block bodies from the peer.
    ///
    /// The response should be sent through the channel.
    GetBlockBodies(GetBlockBodies),
    /// Requests block access lists from the peer.
    ///
    /// The response should be sent through the channel.
    GetBlockAccessLists(GetBlockAccessLists),

    /// Requests receipts from the peer.
    ///
    /// The response should be sent through the channel.
    GetReceipts(GetReceipts),
    /// Requests a `snap/2` (EIP-8189) message from the peer.
    ///
    /// The response should be sent through the channel. Boxed since `SnapProtocolMessage` is
    /// large relative to the other variants.
    GetSnap(Box<SnapProtocolMessage>),
}

/// Corresponding variant for [`PeerRequest`].
#[derive(Debug)]
pub enum PeerResponse {
    /// Represents a response to a request for block headers.
    BlockHeaders {
        /// The receiver channel for the response to a block headers request.
        response: oneshot::Receiver<RequestResult<BlockHeaders<base_common_types_chain::Header>>>,
    },
    /// Represents a response to a request for block bodies.
    BlockBodies {
        /// The receiver channel for the response to a block bodies request.
        response:
            oneshot::Receiver<RequestResult<BlockBodies<base_common_types_chain::BaseBlockBody>>>,
    },
    /// Represents a response to a request for pooled transactions.
    PooledTransactions {
        /// The receiver channel for the response to a pooled transactions request.
        response: oneshot::Receiver<
            RequestResult<PooledTransactions<base_common_types_chain::BasePooledTransaction>>,
        >,
    },
    /// Represents a response to a request for `NodeData`.
    NodeData {
        /// The receiver channel for the response to a `NodeData` request.
        response: oneshot::Receiver<RequestResult<NodeData>>,
    },
    /// Represents a response to a request for receipts.
    Receipts {
        /// The receiver channel for the response to a receipts request.
        response: oneshot::Receiver<RequestResult<Receipts<BaseReceipt>>>,
    },
    /// Represents a response to a request for receipts.
    ///
    /// This is a variant of `Receipts` that was introduced in `eth/69`.
    /// The difference is that this variant does not require the inclusion of bloom filters in the
    /// response, making it more lightweight.
    Receipts69 {
        /// The receiver channel for the response to a receipts request.
        response: oneshot::Receiver<RequestResult<Receipts69<BaseReceipt>>>,
    },
    /// Represents a response to a request for receipts using eth/70.
    Receipts70 {
        /// The receiver channel for the response to a receipts request.
        response: oneshot::Receiver<RequestResult<Receipts70<BaseReceipt>>>,
    },
    /// Represents a response to a request for block access lists.
    BlockAccessLists {
        /// The receiver channel for the response to a block access lists request.
        response: oneshot::Receiver<RequestResult<BlockAccessLists>>,
    },
    ///
    /// Represents a response to a request for cells.
    Cells {
        /// The receiver channel for the response to a cells request.
        response: oneshot::Receiver<RequestResult<Cells>>,
    },
    /// Represents a response to a `snap/2` (EIP-8189) request.
    Snap {
        /// The receiver channel for the response to a `snap/2` request.
        response: oneshot::Receiver<RequestResult<SnapResponse>>,
    },
}

// === impl PeerResponse ===

impl PeerResponse {
    /// Polls the type to completion.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<PeerResponseResult> {
        macro_rules! poll_request {
            ($response:ident, $item:ident, $cx:ident) => {
                match ready!($response.poll_unpin($cx)) {
                    Ok(res) => PeerResponseResult::$item(res.map(|item| item.0)),
                    Err(err) => PeerResponseResult::$item(Err(err.into())),
                }
            };
        }

        let res = match self {
            Self::BlockHeaders { response } => {
                poll_request!(response, BlockHeaders, cx)
            }
            Self::BlockBodies { response } => {
                poll_request!(response, BlockBodies, cx)
            }
            Self::PooledTransactions { response } => {
                poll_request!(response, PooledTransactions, cx)
            }
            Self::NodeData { response } => {
                poll_request!(response, NodeData, cx)
            }
            Self::Receipts { response } => {
                poll_request!(response, Receipts, cx)
            }
            Self::Receipts69 { response } => {
                poll_request!(response, Receipts69, cx)
            }
            Self::Receipts70 { response } => match ready!(response.poll_unpin(cx)) {
                Ok(res) => PeerResponseResult::Receipts70(res),
                Err(err) => PeerResponseResult::Receipts70(Err(err.into())),
            },
            Self::BlockAccessLists { response } => match ready!(response.poll_unpin(cx)) {
                Ok(res) => PeerResponseResult::BlockAccessLists(res),
                Err(err) => PeerResponseResult::BlockAccessLists(Err(err.into())),
            },
            Self::Cells { response } => match ready!(response.poll_unpin(cx)) {
                Ok(res) => PeerResponseResult::Cells(res),
                Err(err) => PeerResponseResult::Cells(Err(err.into())),
            },
            Self::Snap { response } => match ready!(response.poll_unpin(cx)) {
                Ok(res) => PeerResponseResult::Snap(res),
                Err(err) => PeerResponseResult::Snap(Err(err.into())),
            },
        };
        Poll::Ready(res)
    }
}

/// All response variants for [`PeerResponse`]
#[derive(Debug)]
pub enum PeerResponseResult {
    /// Represents a result containing block headers or an error.
    BlockHeaders(RequestResult<Vec<base_common_types_chain::Header>>),
    /// Represents a result containing block bodies or an error.
    BlockBodies(RequestResult<Vec<base_common_types_chain::BaseBlockBody>>),
    /// Represents a result containing pooled transactions or an error.
    PooledTransactions(RequestResult<Vec<base_common_types_chain::BasePooledTransaction>>),
    /// Represents a result containing node data or an error.
    NodeData(RequestResult<Vec<Bytes>>),
    /// Represents a result containing receipts or an error.
    Receipts(RequestResult<Vec<Vec<ReceiptWithBloom<BaseReceipt>>>>),
    /// Represents a result containing receipts or an error for eth/69.
    Receipts69(RequestResult<Vec<Vec<BaseReceipt>>>),
    /// Represents a result containing receipts or an error for eth/70.
    Receipts70(RequestResult<Receipts70<BaseReceipt>>),
    /// Represents a result containing block access lists or an error.
    BlockAccessLists(RequestResult<BlockAccessLists>),
    /// Represents a result containing cells or an error.
    Cells(RequestResult<Cells>),
    /// Represents a result containing a `snap/2` response or an error.
    Snap(RequestResult<SnapResponse>),
}

// === impl PeerResponseResult ===

impl PeerResponseResult {
    /// Converts this response into the [`RequestMessage`] to send back to the peer: an
    /// [`EthMessage`] for every variant except [`Self::Snap`], which becomes a
    /// [`SnapProtocolMessage`].
    pub fn try_into_message(self, id: u64) -> RequestResult<RequestMessage> {
        macro_rules! to_message {
            ($response:ident, $item:ident, $request_id:ident) => {
                match $response {
                    Ok(res) => {
                        let request = RequestPair { request_id: $request_id, message: $item(res) };
                        Ok(RequestMessage::Eth(EthMessage::$item(request)))
                    }
                    Err(err) => Err(err),
                }
            };
        }
        match self {
            Self::BlockHeaders(resp) => {
                to_message!(resp, BlockHeaders, id)
            }
            Self::BlockBodies(resp) => {
                to_message!(resp, BlockBodies, id)
            }
            Self::PooledTransactions(resp) => {
                to_message!(resp, PooledTransactions, id)
            }
            Self::NodeData(resp) => {
                to_message!(resp, NodeData, id)
            }
            Self::Receipts(resp) => {
                to_message!(resp, Receipts, id)
            }
            Self::Receipts69(resp) => {
                to_message!(resp, Receipts69, id)
            }
            Self::Receipts70(resp) => match resp {
                Ok(res) => {
                    let request = RequestPair { request_id: id, message: res };
                    Ok(RequestMessage::Eth(EthMessage::Receipts70(request)))
                }
                Err(err) => Err(err),
            },
            Self::BlockAccessLists(resp) => match resp {
                Ok(res) => {
                    let request = RequestPair { request_id: id, message: res };
                    Ok(RequestMessage::Eth(EthMessage::BlockAccessLists(request)))
                }
                Err(err) => Err(err),
            },
            Self::Cells(resp) => match resp {
                Ok(res) => {
                    let request = RequestPair { request_id: id, message: res };
                    Ok(RequestMessage::Eth(EthMessage::Cells(request)))
                }
                Err(err) => Err(err),
            },
            Self::Snap(resp) => match resp {
                Ok(res) => {
                    let mut message: SnapProtocolMessage = res.into();
                    message.set_request_id(id);
                    Ok(RequestMessage::Snap(message))
                }
                Err(err) => Err(err),
            },
        }
    }

    /// Returns the `Err` value if the result is an error.
    pub fn err(&self) -> Option<&RequestError> {
        match self {
            Self::BlockHeaders(res) => res.as_ref().err(),
            Self::BlockBodies(res) => res.as_ref().err(),
            Self::PooledTransactions(res) => res.as_ref().err(),
            Self::NodeData(res) => res.as_ref().err(),
            Self::Receipts(res) => res.as_ref().err(),
            Self::Receipts69(res) => res.as_ref().err(),
            Self::Receipts70(res) => res.as_ref().err(),
            Self::BlockAccessLists(res) => res.as_ref().err(),
            Self::Cells(res) => res.as_ref().err(),
            Self::Snap(res) => res.as_ref().err(),
        }
    }

    /// Returns whether this result is an error.
    pub fn is_err(&self) -> bool {
        self.err().is_some()
    }
}
