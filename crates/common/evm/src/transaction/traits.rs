//! Contains the transaction trait abstraction.

use auto_impl::auto_impl;
use revm::{
    context_interface::transaction::Transaction,
    primitives::{B256, Bytes, U256},
};

use crate::DEPOSIT_TRANSACTION_TYPE;

use super::Eip8130Parts;

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

    /// EIP-8130 nonce lane key, if present.
    fn aa_nonce_key(&self) -> Option<U256> {
        None
    }

    /// EIP-8130 execution metadata, if present.
    fn eip8130_parts(&self) -> Option<&Eip8130Parts> {
        None
    }

    /// Returns `true` if transaction is of type [`DEPOSIT_TRANSACTION_TYPE`].
    fn is_deposit(&self) -> bool {
        self.tx_type() == DEPOSIT_TRANSACTION_TYPE
    }
}
