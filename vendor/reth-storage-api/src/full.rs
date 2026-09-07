//! Helper trait for full rpc provider

use base_execution_chainspec::ChainSpecProvider;

use crate::{
    BlockReaderIdExt, HeaderProvider, StageCheckpointReader, StateProviderFactory,
    TransactionsProvider,
};

/// Helper trait to unify all provider traits required to support `eth` RPC server behaviour, for
/// simplicity.
pub trait FullRpcProvider:
    StateProviderFactory
    + ChainSpecProvider
    + BlockReaderIdExt
    + HeaderProvider
    + TransactionsProvider
    + StageCheckpointReader
    + Clone
    + Unpin
    + 'static
{
}

impl<T> FullRpcProvider for T where
    T: StateProviderFactory
        + ChainSpecProvider
        + BlockReaderIdExt
        + HeaderProvider
        + TransactionsProvider
        + StageCheckpointReader
        + Clone
        + Unpin
        + 'static
{
}
