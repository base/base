//! Traits describing node builder extensions.

use std::fmt::Debug;

use crate::NodeHooks;

/// Customizes the node builder before launch.
///
/// Register extensions via [`BaseNodeRunner::install_ext`].
pub trait BaseNodeExtension: Send + Sync + Debug {
    /// Applies the extension to the supplied hooks.
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks;
}

/// An extension that can be built from a config.
pub trait FromExtensionConfig: BaseNodeExtension + Sized {
    /// Configuration type used to construct this extension.
    type Config;

    /// Creates a new extension from the provided configuration.
    fn from_config(config: Self::Config) -> Self;
}
