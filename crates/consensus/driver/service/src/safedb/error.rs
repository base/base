//! Error types for the safe head database.

/// Errors from safe head database operations.
#[derive(Debug, thiserror::Error)]
pub enum SafeDBError {
    /// No safe head found at or before the requested L1 block.
    #[error("safe head not found")]
    NotFound,
    /// Safe head tracking is disabled on this node.
    #[error("safe head tracking is disabled")]
    Disabled,
    /// Database error.
    #[error("database error: {0}")]
    Database(String),
}
