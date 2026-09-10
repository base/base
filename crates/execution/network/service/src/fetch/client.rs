//! A client implementation that can interact with the network and download data.

use std::{
    ops::RangeInclusive,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use alloy_primitives::B256;
use base_common_types_chain::BaseReceipt;
use base_execution_network_wire::{
    BalRequirement, BlockAccessLists, BlockAccessListsClient, BlockClient, BodiesClient, BodiesFut,
    DownloadClient, GetAccountRangeMessage, GetBlockAccessListsMessage, GetByteCodesMessage,
    GetStorageRangesMessage, HeadersClient, HeadersRequest, PeerId, PeerRequestResult, Priority,
    ReceiptsClient, ReceiptsFut, ReputationChangeKind, RequestError, SnapClient,
    SnapProtocolMessage, SnapResponse,
};
use futures::{future, future::Either};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::{PeersHandle, fetch::DownloadRequest, flattened_response::FlattenedResponse};

#[cfg_attr(doc, aquamarine::aquamarine)]
/// Front-end API for fetching data from the network.
///
/// Following diagram illustrates how a request, See [`HeadersClient::get_headers`] and
/// [`BodiesClient::get_block_bodies`] is handled internally.
///
/// include_mmd!("docs/mermaid/fetch-client.mmd")
#[derive(Debug, Clone)]
pub struct FetchClient {
    /// Sender half of the request channel.
    pub(crate) request_tx: UnboundedSender<DownloadRequest>,
    /// The handle to the peers
    pub(crate) peers_handle: PeersHandle,
    /// Number of active peer sessions the node's currently handling.
    pub(crate) num_active_peers: Arc<AtomicUsize>,
}

impl DownloadClient for FetchClient {
    fn report_bad_message(&self, peer_id: PeerId) {
        self.peers_handle.reputation_change(peer_id, ReputationChangeKind::BadMessage);
    }

    fn num_connected_peers(&self) -> usize {
        self.num_active_peers.load(Ordering::Relaxed)
    }
}

impl FetchClient {
    /// Sends a `snap/2` request to an available peer.
    fn send_snap_request(
        &self,
        request: SnapProtocolMessage,
        priority: Priority,
    ) -> std::pin::Pin<Box<dyn Future<Output = PeerRequestResult<SnapResponse>> + Send + Sync>>
    {
        let (response, rx) = oneshot::channel();
        if self.request_tx.send(DownloadRequest::GetSnap { request, response, priority }).is_ok() {
            Box::pin(FlattenedResponse::from(rx))
        } else {
            Box::pin(future::err(RequestError::ChannelClosed))
        }
    }
}

// The `Output` future of the [HeadersClient] impl of [FetchClient] that either returns a response
// or an error.
type HeadersClientFuture<T> = Either<FlattenedResponse<T>, future::Ready<T>>;

impl HeadersClient for FetchClient {
    type Output = HeadersClientFuture<PeerRequestResult<Vec<base_common_types_chain::Header>>>;

    /// Sends a `GetBlockHeaders` request to an available peer.
    fn get_headers_with_priority(
        &self,
        request: HeadersRequest,
        priority: Priority,
    ) -> Self::Output {
        let (response, rx) = oneshot::channel();
        if self
            .request_tx
            .send(DownloadRequest::GetBlockHeaders { request, response, priority })
            .is_ok()
        {
            Either::Left(FlattenedResponse::from(rx))
        } else {
            Either::Right(future::err(RequestError::ChannelClosed))
        }
    }
}

impl BodiesClient for FetchClient {
    type Output = BodiesFut;

    /// Sends a `GetBlockBodies` request to an available peer.
    fn get_block_bodies_with_priority_and_range_hint(
        &self,
        request: Vec<B256>,
        priority: Priority,
        range_hint: Option<RangeInclusive<u64>>,
    ) -> Self::Output {
        let (response, rx) = oneshot::channel();
        if self
            .request_tx
            .send(DownloadRequest::GetBlockBodies { request, response, priority, range_hint })
            .is_ok()
        {
            Box::pin(FlattenedResponse::from(rx))
        } else {
            Box::pin(future::err(RequestError::ChannelClosed))
        }
    }
}

impl ReceiptsClient for FetchClient {
    type Receipt = BaseReceipt;
    type Output = ReceiptsFut<BaseReceipt>;

    fn get_receipts_with_priority(&self, request: Vec<B256>, priority: Priority) -> Self::Output {
        let (response, rx) = oneshot::channel();
        if self
            .request_tx
            .send(DownloadRequest::GetReceipts { request, response, priority })
            .is_ok()
        {
            Box::pin(FlattenedResponse::from(rx))
        } else {
            Box::pin(future::err(RequestError::ChannelClosed))
        }
    }
}

impl BlockClient for FetchClient {}

impl BlockAccessListsClient for FetchClient {
    type Output =
        std::pin::Pin<Box<dyn Future<Output = PeerRequestResult<BlockAccessLists>> + Send + Sync>>;

    fn get_block_access_lists_with_priority_and_requirement(
        &self,
        hashes: Vec<B256>,
        priority: Priority,
        requirement: BalRequirement,
    ) -> Self::Output {
        let (response, rx) = oneshot::channel();
        if self
            .request_tx
            .send(DownloadRequest::GetBlockAccessLists {
                request: hashes,
                response,
                priority,
                requirement,
            })
            .is_ok()
        {
            Box::pin(FlattenedResponse::from(rx))
        } else {
            Box::pin(future::err(RequestError::ChannelClosed))
        }
    }
}

impl SnapClient for FetchClient {
    type Output =
        std::pin::Pin<Box<dyn Future<Output = PeerRequestResult<SnapResponse>> + Send + Sync>>;

    /// Sends a `GetAccountRange` (`snap/2`) request to an available peer.
    fn get_account_range_with_priority(
        &self,
        request: GetAccountRangeMessage,
        priority: Priority,
    ) -> Self::Output {
        self.send_snap_request(SnapProtocolMessage::GetAccountRange(request), priority)
    }

    /// Sends a `GetStorageRanges` (`snap/2`) request to an available peer.
    fn get_storage_ranges(&self, request: GetStorageRangesMessage) -> Self::Output {
        self.get_storage_ranges_with_priority(request, Priority::Normal)
    }

    /// Sends a `GetStorageRanges` (`snap/2`) request to an available peer.
    fn get_storage_ranges_with_priority(
        &self,
        request: GetStorageRangesMessage,
        priority: Priority,
    ) -> Self::Output {
        self.send_snap_request(SnapProtocolMessage::GetStorageRanges(request), priority)
    }

    /// Sends a `GetByteCodes` (`snap/2`) request to an available peer.
    fn get_byte_codes(&self, request: GetByteCodesMessage) -> Self::Output {
        self.get_byte_codes_with_priority(request, Priority::Normal)
    }

    /// Sends a `GetByteCodes` (`snap/2`) request to an available peer.
    fn get_byte_codes_with_priority(
        &self,
        request: GetByteCodesMessage,
        priority: Priority,
    ) -> Self::Output {
        self.send_snap_request(SnapProtocolMessage::GetByteCodes(request), priority)
    }

    /// Sends a `GetBlockAccessLists` (`snap/2`) request to an available peer.
    fn get_block_access_lists_with_priority(
        &self,
        request: GetBlockAccessListsMessage,
        priority: Priority,
    ) -> Self::Output {
        self.send_snap_request(SnapProtocolMessage::GetBlockAccessLists(request), priority)
    }
}
