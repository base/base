use std::fmt;

use alloy_rlp::Error as DecoderError;

use crate::{handler::Challenge, node_info::NonContactable};

#[derive(Debug)]
/// A general error that is used throughout the Discv5 library.
pub enum Error {
    /// An invalid ENR was received.
    InvalidEnr,
    /// The public key type is known.
    UnknownPublicKey,
    /// The ENR key used is not supported.
    KeyTypeNotSupported(&'static str),
    /// Failed to derive an ephemeral public key.
    KeyDerivationFailed,
    /// The remote's public key was invalid.
    InvalidRemotePublicKey,
    /// The secret key does not match the provided ENR.
    InvalidSecretKey,
    /// An invalid signature was received for a challenge.
    InvalidChallengeSignature(Box<Challenge>),
    /// The Service channel has been closed early.
    ServiceChannelClosed,
    /// The discv5 service is not running.
    ServiceNotStarted,
    /// The service has is already running.
    ServiceAlreadyStarted,
    /// A session could not be established with the remote.
    SessionNotEstablished,
    /// An RLP decoding error occurred.
    RLPError(DecoderError),
    /// Failed to encrypt a message.
    EncryptionFail(String),
    /// Failed to decrypt a message.
    DecryptionFailed(String),
    /// The custom error has occurred.
    Custom(&'static str),
    /// A generic dynamic error occurred.
    Error(String),
    /// An IO error occurred.
    Io(std::io::Error),
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Error {
        Error::Io(err)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Types of packet errors.
pub enum PacketError {
    /// The packet type is unknown.
    UnknownPacket,
    /// The packet size was larger than expected.
    TooLarge,
    /// The packet size was smaller than expected.
    TooSmall,
    /// The NodeId sent was invalid.
    InvalidNodeId,
    /// The header has an invalid length.
    HeaderLengthInvalid(usize),
    /// The header could not be decrypted.
    HeaderDecryptionFailed,
    /// The authdata size is too large.
    InvalidAuthDataSize,
    /// The handshake is of an invalid version.
    InvalidVersion(u16),
    /// The ENR sent was invalid.
    InvalidEnr(DecoderError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResponseError {
    /// The channel used to send the response has already been closed.
    ChannelClosed,
}

impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResponseError::ChannelClosed => {
                write!(f, "response channel has already been closed")
            }
        }
    }
}

impl std::error::Error for ResponseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// The request timed out.
    Timeout,
    /// The discovery service has not been started.
    ServiceNotStarted,
    /// The request was sent to ourselves.
    SelfRequest,
    /// The channel to the underlying threads failed.
    ChannelFailed(String),
    /// An invalid ENR was provided.
    InvalidEnr(&'static str),
    /// The remote's ENR is invalid.
    InvalidRemoteEnr,
    /// The remote returned and invalid packet.
    InvalidRemotePacket,
    /// Failed attempting to encrypt the request.
    EncryptionFailed(String),
    /// The multiaddr provided is invalid.
    InvalidMultiaddr(&'static str),
    /// Failure generating random numbers during request.
    EntropyFailure(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// The discv5 service is not currently running.
    ServiceNotStarted,
    /// The channel to the underlying threads failed.
    ChannelFailed(String),
    /// The ENR provided was invalid.
    InvalidEnr(String),
    /// Encrypting the message failed.
    EncryptionFailed(String),
    /// The multiaddr provided was invalid.
    InvalidMultiaddr(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl From<NonContactable> for RequestError {
    fn from(_: NonContactable) -> Self {
        RequestError::InvalidEnr("ENR is not contactable")
    }
}
