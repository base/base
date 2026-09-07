//! Precompile dispatch interface.

use alloc::{boxed::Box, vec::Vec};

use alloy_primitives::{Address, Bytes};
use auto_impl::auto_impl;

use super::{Evm, NonStaticAny};
use crate::{
    EvmTypesHost, PrecompileError,
    interpreter::{GasTracker, Message},
    precompiles::PrecompileId,
};

/// Result returned by a precompile.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct PrecompileOutput {
    /// Returned bytes.
    bytes: Bytes,
}

impl PrecompileOutput {
    /// Creates a new precompile output.
    #[inline]
    pub const fn new(bytes: Bytes) -> Self {
        Self { bytes }
    }

    /// Returns the output bytes.
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }

    /// Consumes the output and returns its bytes.
    #[inline]
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }
}

/// Precompile execution hook.
#[auto_impl(&mut, Box)]
pub trait PrecompileProvider<T: EvmTypesHost>: NonStaticAny {
    /// Returns precompile addresses.
    fn addresses(&self) -> Vec<Address> {
        Vec::new()
    }

    /// Returns precompile addresses and identifiers.
    fn precompile_ids(&self) -> Vec<(Address, PrecompileId)> {
        Vec::new()
    }

    /// Returns whether `address` has a registered precompile.
    fn contains(&self, address: &Address) -> bool;

    /// Executes the precompile at `address`, if one is registered.
    fn execute(
        &mut self,
        evm: &mut Evm<'_, T>,
        message: &Message<T>,
        gas: &mut GasTracker,
    ) -> Option<Result<PrecompileOutput, PrecompileError>>;
}

#[inline]
pub(crate) fn boxed_precompile_provider<'a, T: EvmTypesHost>(
    precompiles: impl PrecompileProvider<T> + 'a,
) -> Box<dyn PrecompileProvider<T> + 'a> {
    Box::new(precompiles)
}

impl<'a, T: EvmTypesHost> core::ops::Deref for dyn PrecompileProvider<T> + 'a {
    type Target = dyn NonStaticAny + 'a;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self
    }
}

impl<'a, T: EvmTypesHost> core::ops::DerefMut for dyn PrecompileProvider<T> + 'a {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self
    }
}

/// Empty precompile provider.
#[allow(missing_copy_implementations)]
#[derive(Clone, Debug, Default)]
pub struct NoPrecompiles(());

impl<T: EvmTypesHost> PrecompileProvider<T> for NoPrecompiles {
    #[inline]
    fn addresses(&self) -> Vec<Address> {
        Vec::new()
    }

    #[inline]
    fn contains(&self, _address: &Address) -> bool {
        false
    }

    #[inline]
    fn execute(
        &mut self,
        _evm: &mut Evm<'_, T>,
        _message: &Message<T>,
        _gas: &mut GasTracker,
    ) -> Option<Result<PrecompileOutput, PrecompileError>> {
        None
    }
}
