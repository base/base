//! Sequencer administration errors.

use thiserror::Error;

/// Errors that can occur when using the sequencer admin API.
#[derive(Debug, Error)]
pub enum SequencerAdminAPIError {
    /// Error sending request.
    #[error("Error sending request: {0}.")]
    RequestError(String),

    /// Error receiving response.
    /// Note: this error message is not future-proof, in that it may not be a safe assumption that
    /// communication is channel-based. If/when that changes the enum will likely need to be updated
    /// to take a parameter, so we can change it then.
    #[error("Error receiving response: response channel closed.")]
    ResponseError,

    /// Sequencer stopped successfully, followed by some error.
    #[error("Sequencer stopped successfully, followed by error: {0}.")]
    ErrorAfterSequencerWasStopped(String),

    /// Error overriding leader.
    #[error("Error overriding leader: {0}.")]
    LeaderOverrideError(String),

    /// Node is not the conductor leader and cannot start sequencing.
    #[error("Node is not the conductor leader.")]
    NotLeader,
}
