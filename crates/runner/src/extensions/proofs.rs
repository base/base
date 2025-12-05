//! Contains a [`ProofsExtensions`] which stores intermediate storage updates.

use crate::{ProofsCell, ProofsConfig, extensions::OpBuilder};

/// Proofs Extension.
#[derive(Debug, Clone)]
pub struct ProofsExtension<S> {
    /// Shared state.
    pub cell: ProofsCell<S>,
    /// Proofs config.
    pub config: ProofsConfig,
}

impl<S> ProofsExtension<S> {
    /// Create a new Proofs extension helper.
    pub const fn new(cell: ProofsCell<S>, config: ProofsConfig) -> Self {
        Self { cell, config }
    }

    /// Applies the extension to the supplied builder.
    pub fn apply(&self, _builder: OpBuilder) -> OpBuilder {
        unimplemented!("ProofsExtension::apply is not yet implemented")
    }
}
