//! Helpers for recovering signers from a set of transactions

use alloc::vec::Vec;

use alloy_primitives::Address;
use base_common_types_chain::transaction::SignerRecoverable;
#[cfg(feature = "rayon")]
use rayon::prelude::{IntoParallelIterator, ParallelIterator};

use crate::sealed_block::{Recovered, RecoveryError, SignedTransaction};

/// Recovers a list of signers from a transaction list iterator.
///
/// Returns `Err(RecoveryError)`, if some transaction's signature is invalid.
///
/// When the `rayon` feature is enabled, recovery is performed in parallel.
#[cfg(feature = "rayon")]
pub fn recover_signers<'a, I, T>(txes: I) -> Result<Vec<Address>, RecoveryError>
where
    T: SignedTransaction,
    I: IntoParallelIterator<Item = &'a T>,
{
    txes.into_par_iter().map(|tx| tx.recover_signer()).collect()
}

/// Recovers a list of signers from a transaction list iterator.
///
/// Returns `Err(RecoveryError)`, if some transaction's signature is invalid.
#[cfg(not(feature = "rayon"))]
pub fn recover_signers<'a, I, T>(txes: I) -> Result<Vec<Address>, RecoveryError>
where
    T: SignedTransaction,
    I: IntoIterator<Item = &'a T>,
{
    txes.into_iter().map(|tx| tx.recover_signer()).collect()
}

/// Recovers a list of signers from a transaction list iterator _without ensuring that the
/// signature has a low `s` value_.
///
/// Returns `Err(RecoveryError)`, if some transaction's signature is invalid.
///
/// When the `rayon` feature is enabled, recovery is performed in parallel.
#[cfg(feature = "rayon")]
pub fn recover_signers_unchecked<'a, I, T>(txes: I) -> Result<Vec<Address>, RecoveryError>
where
    T: SignedTransaction,
    I: IntoParallelIterator<Item = &'a T>,
{
    txes.into_par_iter().map(|tx| tx.recover_signer_unchecked()).collect()
}

/// Recovers a list of signers from a transaction list iterator _without ensuring that the
/// signature has a low `s` value_.
///
/// Returns `Err(RecoveryError)`, if some transaction's signature is invalid.
#[cfg(not(feature = "rayon"))]
pub fn recover_signers_unchecked<'a, I, T>(txes: I) -> Result<Vec<Address>, RecoveryError>
where
    T: SignedTransaction,
    I: IntoIterator<Item = &'a T>,
{
    txes.into_iter().map(|tx| tx.recover_signer_unchecked()).collect()
}

/// Decodes and recovers a list of [`Recovered`] transactions from an iterator.
///
/// The `decode` closure transforms each item into a [`SignedTransaction`], which is then
/// recovered.
///
/// Returns an error if decoding or signature recovery fails for any transaction.
///
/// When the `rayon` feature is enabled, recovery is performed in parallel.
#[cfg(feature = "rayon")]
pub fn try_recover_signers<I, F, T>(items: I, decode: F) -> Result<Vec<Recovered<T>>, RecoveryError>
where
    I: IntoParallelIterator,
    F: Fn(I::Item) -> Result<T, RecoveryError> + Sync,
    T: SignedTransaction,
{
    items
        .into_par_iter()
        .map(|item| {
            let tx = decode(item)?;
            SignerRecoverable::try_into_recovered(tx)
        })
        .collect()
}

/// Decodes and recovers a list of [`Recovered`] transactions from an iterator.
///
/// The `decode` closure transforms each item into a [`SignedTransaction`], which is then
/// recovered.
///
/// Returns an error if decoding or signature recovery fails for any transaction.
#[cfg(not(feature = "rayon"))]
pub fn try_recover_signers<I, F, T>(items: I, decode: F) -> Result<Vec<Recovered<T>>, RecoveryError>
where
    I: IntoIterator,
    F: Fn(I::Item) -> Result<T, RecoveryError>,
    T: SignedTransaction,
{
    items
        .into_iter()
        .map(|item| {
            let tx = decode(item)?;
            SignerRecoverable::try_into_recovered(tx)
        })
        .collect()
}
