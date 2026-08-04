//! Traits describing node builder extensions.

use std::fmt::{self, Debug};

use crate::NodeHooks;

/// Customizes the node builder before launch.
///
/// Generic over the pool extension type `E` (defaulting to `()`), matching the
/// [`NodeHooks<E>`] the runner threads through.
///
/// Register extensions via [`BaseNodeRunner::install_ext`].
pub trait BaseNodeExtension<E = ()>: Send + Sync + Debug
where
    E: fmt::Debug + Clone + Send + Sync + Unpin + 'static,
{
    /// Applies the extension to the supplied hooks.
    fn apply(self: Box<Self>, hooks: NodeHooks<E>) -> NodeHooks<E>;
}

/// An extension that can be built from a config.
pub trait FromExtensionConfig<E = ()>: BaseNodeExtension<E> + Sized
where
    E: fmt::Debug + Clone + Send + Sync + Unpin + 'static,
{
    /// Configuration type used to construct this extension.
    type Config;

    /// Creates a new extension from the provided configuration.
    fn from_config(config: Self::Config) -> Self;
}
