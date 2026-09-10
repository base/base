use base_common_types_chain::SealedHeader;
use base_execution_evm_blocks::ConsensusError;
use derive_more::{Display, Error};

/// Header downloader result
pub type HeadersDownloaderResult<T> = Result<T, HeadersDownloaderError>;

/// Error variants that can happen when sending requests to a session.
#[derive(Debug, Clone, Display, Error)]
pub enum HeadersDownloaderError {
    /// The downloaded header cannot be attached to the local head,
    /// but is valid otherwise.
    #[display("valid downloaded header cannot be attached to the local head: {error}")]
    DetachedHead {
        /// The local head we attempted to attach to.
        local_head: Box<SealedHeader>,
        /// The header we attempted to attach.
        header: Box<SealedHeader>,
        /// The error that occurred when attempting to attach the header.
        #[error(source)]
        error: Box<ConsensusError>,
    },
}
