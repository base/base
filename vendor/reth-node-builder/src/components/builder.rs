//! Component construction callback used during node launch.

use std::fmt;

use futures::future::BoxFuture;
use reth_db_api::{Database, database_metrics::DatabaseMetrics};

use crate::BuilderContext;

/// A single-use callback that constructs the components for a node launch.
///
/// Base supplies its concrete component assembly through this callback. The callback
/// also supports launch hooks without a separate component-builder trait.
pub struct ComponentBuilder<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> {
    /// Constructs the components using the launch context.
    pub build: Box<
        dyn for<'a> FnOnce(
                &'a BuilderContext<DB>,
            )
                -> BoxFuture<'a, eyre::Result<base_node_context::BaseNodeContext<DB>>>
            + Send,
    >,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> fmt::Debug for ComponentBuilder<DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComponentBuilder").finish_non_exhaustive()
    }
}
