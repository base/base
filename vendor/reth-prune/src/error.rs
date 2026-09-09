use base_execution_state_types::DatabaseError;
use base_execution_state_types::PruneSegmentError;
use base_execution_state_provider::ProviderError;
use thiserror::Error;

/// Errors that can occur during pruning.
#[derive(Error, Debug)]
pub enum PrunerError {
    #[error(transparent)]
    PruneSegment(#[from] PruneSegmentError),

    #[error("inconsistent data: {0}")]
    InconsistentData(&'static str),

    #[error(transparent)]
    Database(#[from] DatabaseError),

    #[error(transparent)]
    Provider(#[from] ProviderError),
}
