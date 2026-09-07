//! The error type emitted for various ENR operations.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
/// An error type for handling various ENR operations.
pub enum Error {
    /// The ENR is too large.
    ExceedsMaxSize,
    /// The sequence number is too large.
    SequenceNumberTooHigh,
    /// There was an error with signing an ENR record.
    SigningError,
    /// The identity scheme is not supported.
    UnsupportedIdentityScheme,
    /// Failed decoding the RLP data.
    InvalidRlpData(alloy_rlp::Error),
}

impl From<alloy_rlp::Error> for Error {
    fn from(decode_error: alloy_rlp::Error) -> Self {
        Error::InvalidRlpData(decode_error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExceedsMaxSize => write!(f, "enr exceeds max size"),
            Self::SequenceNumberTooHigh => write!(f, "sequence number too large"),
            Self::SigningError => write!(f, "signing error"),
            Self::UnsupportedIdentityScheme => write!(f, "unsupported identity scheme"),
            Self::InvalidRlpData(_rlp) => write!(f, "invalid rlp data"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::ExceedsMaxSize
            | Error::SequenceNumberTooHigh
            | Error::SigningError
            | Error::UnsupportedIdentityScheme => None,
            Error::InvalidRlpData(e) => Some(e),
        }
    }
}
