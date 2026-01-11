//! Traits describing node builder extensions.

use std::fmt::Debug;

use crate::OpBuilder;

/// A node builder extension that can apply additional wiring to the builder.
pub trait BaseNodeExtension: Send + Sync + Debug {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder;
}
