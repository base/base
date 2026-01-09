//! Traits describing configurable node builder extensions.

use std::fmt::Debug;

use eyre::Result;

use crate::OpBuilder;

/// A node builder extension that can apply additional wiring to the builder.
pub trait BaseNodeExtension: Send + Sync + Debug {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder;
}

/// An extension that can be constructed from a configuration type.
pub trait ConfigurableBaseNodeExtension<C>: BaseNodeExtension + Sized + 'static {
    /// Builds the extension from the node config.
    fn build(config: &C) -> Result<Self>;
}
