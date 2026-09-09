//! Component construction callback used during node launch.

use std::fmt;

use futures::future::BoxFuture;

use crate::BuilderContext;

/// A single-use callback that constructs the components for a node launch.
///
/// Base supplies its concrete component assembly through this callback. The callback
/// also supports launch hooks without a separate component-builder trait.
pub struct ComponentBuilder {
    /// Constructs the components using the launch context.
    pub build: Box<
        dyn for<'a> FnOnce(
                &'a BuilderContext,
            )
                -> BoxFuture<'a, eyre::Result<base_node_context::BaseNodeContext>>
            + Send,
    >,
}

impl fmt::Debug for ComponentBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComponentBuilder").finish_non_exhaustive()
    }
}
