//! Access to a provider's Base chain configuration.

use alloc::sync::Arc;
use core::fmt::Debug;

use crate::BaseChainSpec;

/// Reads the runtime chain configuration used by a provider.
#[auto_impl::auto_impl(&, Arc)]
pub trait ChainSpecProvider: Debug + Send {
    /// Returns the provider's Base chain configuration.
    fn chain_spec(&self) -> Arc<BaseChainSpec>;
}
