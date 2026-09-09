use base_execution_state_types::PruneSegmentError;
use reth_provider::ProviderError;
use reth_storage_errors::db::DatabaseError;
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
