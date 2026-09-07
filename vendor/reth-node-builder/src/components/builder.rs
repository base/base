//! Component construction callback used during node launch.

use std::fmt;

use futures::future::BoxFuture;

use crate::{BuilderContext, FullNodeTypes};

/// A single-use callback that constructs the components for a node launch.
///
/// Base supplies its concrete component assembly through this callback. The callback
/// also supports launch hooks without a separate component-builder trait.
pub struct ComponentBuilder<Node: FullNodeTypes, C> {
    /// Constructs the components using the launch context.
    pub build:
        Box<dyn for<'a> FnOnce(&'a BuilderContext<Node>) -> BoxFuture<'a, eyre::Result<C>> + Send>,
}

impl<Node: FullNodeTypes, C> fmt::Debug for ComponentBuilder<Node, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComponentBuilder").finish_non_exhaustive()
    }
}
