use crate::HeadersRequest;
use alloy_eips::BlockHashOrNumber;
use base_common_types_chain::BlockHeader;
use base_execution_network_types::{ReputationChangeKind, WithPeerId};
use derive_more::{Display, Error};
use tokio::sync::{mpsc, oneshot};

/// Result alias for result of a request.
pub type RequestResult<T> = Result<T, RequestError>;

/// Result with [`PeerId`][base_execution_network_types::PeerId]
pub type PeerRequestResult<T> = RequestResult<WithPeerId<T>>;

/// Helper trait used to validate responses.
pub trait EthResponseValidator {
    /// Determine whether the response matches what we requested in [`HeadersRequest`]
    fn is_likely_bad_headers_response(&self, request: &HeadersRequest) -> bool;

    /// Return the response reputation impact if any
    fn reputation_change_err(&self) -> Option<ReputationChangeKind>;
}

impl<H: BlockHeader> EthResponseValidator for RequestResult<Vec<H>> {
    fn is_likely_bad_headers_response(&self, request: &HeadersRequest) -> bool {
        match self {
            Ok(headers) => {
                let request_length = headers.len() as u64;

                if (request_length == 0 && request.limit > 0) || request_length > request.limit {
                    return true;
                }

                match request.start {
                    BlockHashOrNumber::Number(block_number) => {
                        headers.first().is_some_and(|header| block_number != header.number())
                    }
                    BlockHashOrNumber::Hash(_) => {
                        // we don't want to hash the header
                        false
                    }
                }
            }
            Err(_) => true,
        }
    }

    /// [`RequestError::ChannelClosed`] is not possible here since these errors are mapped to
    /// `ConnectionDropped`, which will be handled when the dropped connection is cleaned up.
    ///
    /// [`RequestError::ConnectionDropped`] should be ignored here because this is already handled
    /// when the dropped connection is handled.
    ///
    /// [`RequestError::UnsupportedCapability`] is also used for locally rejected optional requests,
    /// which should not affect peer reputation.
    fn reputation_change_err(&self) -> Option<ReputationChangeKind> {
        if let Err(err) = self {
            match err {
                RequestError::ChannelClosed
                | RequestError::ConnectionDropped
                | RequestError::UnsupportedCapability
                | RequestError::BadResponse => None,
                RequestError::Timeout => Some(ReputationChangeKind::Timeout),
            }
        } else {
            None
        }
    }
}

/// Error variants that can happen when sending requests to a session.
///
/// Represents errors encountered when sending requests.
#[derive(Clone, Debug, Eq, PartialEq, Display, Error)]
pub enum RequestError {
    /// Closed channel to the peer.
    /// Indicates the channel to the peer is closed.
    #[display("closed channel to the peer")]
    ChannelClosed,
    /// Connection to a peer dropped while handling the request.
    /// Represents a dropped connection while handling the request.
    #[display("connection to a peer dropped while handling the request")]
    ConnectionDropped,
    /// Capability message is not supported by the remote peer.
    /// Indicates an unsupported capability message from the remote peer.
    #[display("capability message is not supported by remote peer")]
    UnsupportedCapability,
    /// Request timed out while awaiting response.
    /// Represents a timeout while waiting for a response.
    #[display("request timed out while awaiting response")]
    Timeout,
    /// Received bad response.
    /// Indicates a bad response was received.
    #[display("received bad response")]
    BadResponse,
}

// === impl RequestError ===

impl RequestError {
    /// Indicates whether this error is retryable or fatal.
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout | Self::ConnectionDropped)
    }

    /// Whether the error happened because the channel was closed.
    pub const fn is_channel_closed(&self) -> bool {
        matches!(self, Self::ChannelClosed)
    }
}

impl<T> From<mpsc::error::SendError<T>> for RequestError {
    fn from(_: mpsc::error::SendError<T>) -> Self {
        Self::ChannelClosed
    }
}

impl From<oneshot::error::RecvError> for RequestError {
    fn from(_: oneshot::error::RecvError) -> Self {
        Self::ChannelClosed
    }
}

#[cfg(test)]
mod tests {
    use base_common_types_chain::Header;

    use super::*;

    #[test]
    fn test_is_likely_bad_headers_response() {
        let request =
            HeadersRequest { start: 0u64.into(), limit: 0, direction: Default::default() };
        let headers: Vec<Header> = vec![];
        assert!(!Ok(headers).is_likely_bad_headers_response(&request));

        let request =
            HeadersRequest { start: 0u64.into(), limit: 1, direction: Default::default() };
        let headers: Vec<Header> = vec![];
        assert!(Ok(headers).is_likely_bad_headers_response(&request));

        let request =
            HeadersRequest { start: 0u64.into(), limit: 2, direction: Default::default() };
        let headers = vec![Header::default()];
        assert!(!Ok(headers).is_likely_bad_headers_response(&request));

        let request =
            HeadersRequest { start: 0u64.into(), limit: 1, direction: Default::default() };
        let headers = vec![Header::default(), Header::default()];
        assert!(Ok(headers).is_likely_bad_headers_response(&request));
    }
}
