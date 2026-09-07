mod macros;

mod ethereum;
pub use ethereum::*;

use alloc::boxed::Box;
use core::{
    any::Any,
    hash::{Hash, Hasher},
};
use dyn_clone::DynClone;

/// Generic hardfork trait.
#[auto_impl::auto_impl(&, Box)]
pub trait Hardfork: Any + DynClone + Send + Sync + 'static {
    /// Fork name.
    fn name(&self) -> &'static str;

    /// Returns boxed value.
    fn boxed(&self) -> Box<dyn Hardfork + '_> {
        Box::new(self)
    }
}

dyn_clone::clone_trait_object!(Hardfork);

impl core::fmt::Debug for dyn Hardfork + 'static {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct(self.name()).finish()
    }
}

impl PartialEq for dyn Hardfork + 'static {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

impl Eq for dyn Hardfork + 'static {}

impl Hash for dyn Hardfork + 'static {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name().hash(state)
    }
}
