//! Contains the transaction trait abstraction.

use auto_impl::auto_impl;
use revm::{
    context_interface::transaction::Transaction,
    primitives::{B256, Bytes},
};

use crate::{DEPOSIT_TRANSACTION_TYPE, EIP8130_TRANSACTION_TYPE, Eip8130TransactionParts};

/// Base Transaction trait.
#[auto_impl(&, &mut, Box, Arc)]
pub trait BaseTxTr: Transaction {
    /// Enveloped transaction bytes.
    fn enveloped_tx(&self) -> Option<&Bytes>;

    /// Source hash of the deposit transaction.
    fn source_hash(&self) -> Option<B256>;

    /// Mint of the deposit transaction
    fn mint(&self) -> Option<u128>;

    /// Whether the transaction is a system transaction
    fn is_system_transaction(&self) -> bool;

    /// The EIP-8130 account-abstraction parts (the signed envelope), or `None`
    /// for every other transaction type.
    fn eip8130_parts(&self) -> Option<&Eip8130TransactionParts>;

    /// Returns `true` if transaction is of type [`DEPOSIT_TRANSACTION_TYPE`].
    fn is_deposit(&self) -> bool {
        self.tx_type() == DEPOSIT_TRANSACTION_TYPE
    }

    /// Returns `true` if transaction is of type [`EIP8130_TRANSACTION_TYPE`].
    fn is_eip8130(&self) -> bool {
        self.tx_type() == EIP8130_TRANSACTION_TYPE
    }
}
