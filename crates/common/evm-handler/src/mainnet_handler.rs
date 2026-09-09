use base_evm_context::{ContextTr, HaltReason};

use super::{EvmTrError, Handler};
use crate::EvmTr;

/// Mainnet handler that implements the default [`Handler`] trait for the Evm.
#[derive(Debug, Clone)]
pub struct MainnetHandler<CTX, ERROR> {
    /// Phantom data to hold the generic type parameters.
    pub _phantom: core::marker::PhantomData<(CTX, ERROR)>,
}

impl<EVM, ERROR> Handler for MainnetHandler<EVM, ERROR>
where
    EVM: EvmTr<Context: ContextTr>,
    ERROR: EvmTrError<EVM>,
{
    type Evm = EVM;
    type Error = ERROR;
    type HaltReason = HaltReason;
}

impl<CTX, ERROR> Default for MainnetHandler<CTX, ERROR> {
    fn default() -> Self {
        Self { _phantom: core::marker::PhantomData }
    }
}
