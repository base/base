use crate::NoopFullBlockClient;
use base_execution_network_wire::{
    GetAccountRangeMessage, GetBlockAccessListsMessage, GetByteCodesMessage,
    GetStorageRangesMessage, PeerRequestResult, Priority, RequestError, SnapClient, SnapResponse,
};

/// Fails every snap request with [`RequestError::UnsupportedCapability`], so the noop client can
/// stand in wherever a [`SnapClient`] bound is required but snap is not served.
impl SnapClient for NoopFullBlockClient {
    type Output = futures::future::Ready<PeerRequestResult<SnapResponse>>;

    /// Fails the account range request as unsupported.
    fn get_account_range_with_priority(
        &self,
        _request: GetAccountRangeMessage,
        _priority: Priority,
    ) -> Self::Output {
        unsupported()
    }

    /// Fails the storage ranges request as unsupported.
    fn get_storage_ranges(&self, _request: GetStorageRangesMessage) -> Self::Output {
        unsupported()
    }

    /// Fails the prioritized storage ranges request as unsupported.
    fn get_storage_ranges_with_priority(
        &self,
        _request: GetStorageRangesMessage,
        _priority: Priority,
    ) -> Self::Output {
        unsupported()
    }

    /// Fails the bytecode request as unsupported.
    fn get_byte_codes(&self, _request: GetByteCodesMessage) -> Self::Output {
        unsupported()
    }

    /// Fails the prioritized bytecode request as unsupported.
    fn get_byte_codes_with_priority(
        &self,
        _request: GetByteCodesMessage,
        _priority: Priority,
    ) -> Self::Output {
        unsupported()
    }

    /// Fails the block access lists request as unsupported.
    fn get_block_access_lists_with_priority(
        &self,
        _request: GetBlockAccessListsMessage,
        _priority: Priority,
    ) -> Self::Output {
        unsupported()
    }
}

/// The noop answer to any snap request: immediately ready, no capability.
fn unsupported() -> futures::future::Ready<PeerRequestResult<SnapResponse>> {
    futures::future::ready(Err(RequestError::UnsupportedCapability))
}
