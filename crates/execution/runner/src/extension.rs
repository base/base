//! Traits describing node builder extensions.

use std::{fmt::Debug, sync::Arc};

use base_execution_txpool::{BasePooledTransaction, SidecarPool};

use crate::NodeHooks;

/// Customizes the node builder before launch.
///
/// Register extensions via [`BaseNodeRunner::install_ext`].
pub trait BaseNodeExtension: Send + Sync + Debug {
    /// Applies the extension to the supplied hooks.
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks;

    /// Sidecar sub-pools this extension contributes to the transaction pool.
    ///
    /// Called by the runner *before* components are built, and therefore before
    /// [`apply`](Self::apply) consumes the boxed extension. Implementors must construct the pool
    /// in their constructor and hand out `Arc` clones here.
    fn sidecar_pools(&self) -> Vec<Arc<dyn SidecarPool<BasePooledTransaction>>> {
        Vec::new()
    }
}

/// An extension that can be built from a config.
pub trait FromExtensionConfig: BaseNodeExtension + Sized {
    /// Configuration type used to construct this extension.
    type Config;

    /// Creates a new extension from the provided configuration.
    fn from_config(config: Self::Config) -> Self;
}
