//! Transaction abstraction
//!
//! This module provides traits for working with blockchain transactions:
//! - [`Transaction`] - Basic transaction interface
//! - [`signed::SignedTransaction`] - Transaction with signature and recovery methods
//!
//! # Transaction Recovery
//!
//! Transaction senders are not stored directly but recovered from signatures.
//! Use `recover_signer` for post-EIP-2 transactions or `recover_signer_unchecked`
//! for historical transactions.

pub mod signature;
pub mod signed;

pub mod error;
pub mod recover;

use core::{fmt, hash::Hash};

pub use base_common_types_chain::transaction::{
    SignerRecoverable, TransactionInfo, TransactionMeta, TxHashRef,
};

use crate::{InMemorySize, MaybeSerde};

#[cfg(all(test, feature = "std", feature = "reth-codec"))]
mod access_list;

/// Abstraction of a transaction.
pub trait Transaction:
    Send
    + Sync
    + Unpin
    + Clone
    + fmt::Debug
    + Eq
    + PartialEq
    + Hash
    + base_common_types_chain::Transaction
    + InMemorySize
    + MaybeSerde
{
}

impl<T> Transaction for T where
    T: Send
        + Sync
        + Unpin
        + Clone
        + fmt::Debug
        + Eq
        + PartialEq
        + Hash
        + base_common_types_chain::Transaction
        + InMemorySize
        + MaybeSerde
{
}
