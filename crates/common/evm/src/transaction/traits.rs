//! Contains the transaction trait abstraction.

use auto_impl::auto_impl;
use revm::{
    context_interface::transaction::Transaction,
    primitives::{B256, Bytes},
};

use crate::{BASE_TRANSACTION_TYPE, DEPOSIT_TRANSACTION_TYPE, TxBaseTransactionParts};

/// Base Transaction trait.
#[auto_impl(&, &mut, Box, Arc)]
pub trait BaseTxTr: Transaction {
    /// Enveloped transaction bytes.
    fn enveloped_tx(&self) -> Option<&Bytes>;

    /// `FastLZ` compressed size of [`Self::enveloped_tx`], if already computed, so the L1 cost
    /// does not recompute it.
    fn enveloped_tx_fastlz_size(&self) -> Option<u32> {
        None
    }

    /// Source hash of the deposit transaction.
    fn source_hash(&self) -> Option<B256>;

    /// Mint of the deposit transaction
    fn mint(&self) -> Option<u128>;

    /// Whether the transaction is a system transaction
    fn is_system_transaction(&self) -> bool;

    /// The Base transaction account-abstraction parts (the signed envelope), or `None`
    /// for every other transaction type.
    fn base_tx_parts(&self) -> Option<&TxBaseTransactionParts>;

    /// Returns `true` if transaction is of type [`DEPOSIT_TRANSACTION_TYPE`].
    fn is_deposit(&self) -> bool {
        self.tx_type() == DEPOSIT_TRANSACTION_TYPE
    }

    /// Returns `true` if transaction is of type [`BASE_TRANSACTION_TYPE`].
    fn is_base_tx(&self) -> bool {
        self.tx_type() == BASE_TRANSACTION_TYPE
    }

    /// Returns `true` if this is a read-only Base transaction simulation
    /// (`eth_estimateGas` / `eth_call`) that must be routed to
    /// `TxBaseExecutor::simulate` rather than `execute`.
    fn is_base_tx_simulate(&self) -> bool {
        self.base_tx_parts().is_some_and(|parts| parts.mode.is_simulate())
    }
}
