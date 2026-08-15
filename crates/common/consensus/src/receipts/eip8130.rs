//! EIP-8130 (account-abstraction) receipt type for Base chains.
//!
//! [`Eip8130Receipt`] is a standard [`Receipt`] augmented with the per-phase
//! execution statuses (`phaseStatuses`) defined by EIP-8130. The phase statuses
//! are *not* part of the consensus receipt: they are excluded from the RLP
//! encoding (so the receipts-trie root is unchanged and stays compatible with
//! the standard receipt body) and from the consensus JSON, and are surfaced only
//! through `eth_getTransactionReceipt`. They are persisted in the node's local
//! database via the `Compact` encoding so the RPC layer can read them back.

use alloc::vec::Vec;

use alloy_consensus::{InMemorySize, Receipt};
use alloy_primitives::Log;

/// EIP-8130 account-abstraction receipt: a standard [`Receipt`] plus the
/// per-phase execution statuses.
///
/// Each entry of `phase_statuses` is `0x01` (the phase committed) or `0x00` (the
/// phase reverted, or was skipped because an earlier phase reverted). It is empty
/// when the transaction carried no `calls`. The overall [`Receipt::status`]
/// reports `true` only when every phase succeeded (or `calls` was empty),
/// matching the EIP-8130 receipt `status` semantics.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Eip8130Receipt<T = Log> {
    /// The inner (standard) receipt: status, cumulative gas used, and logs. This
    /// is the only part committed to the consensus receipt encoding.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub inner: Receipt<T>,
    /// Per-phase execution statuses (`0x01` success, `0x00` reverted/skipped).
    ///
    /// Skipped by the consensus JSON and RLP encodings; surfaced only at the RPC
    /// layer and persisted via the `Compact` database encoding.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub phase_statuses: Vec<u8>,
}

impl<T> Eip8130Receipt<T> {
    /// Creates a new [`Eip8130Receipt`] from an inner receipt and its per-phase
    /// statuses.
    pub const fn new(inner: Receipt<T>, phase_statuses: Vec<u8>) -> Self {
        Self { inner, phase_statuses }
    }

    /// Consumes the type and returns the inner [`Receipt`].
    pub fn into_inner(self) -> Receipt<T> {
        self.inner
    }

    /// Maps the inner receipt, preserving the per-phase statuses.
    pub fn map_inner<U, F>(self, f: F) -> Eip8130Receipt<U>
    where
        F: FnOnce(Receipt<T>) -> Receipt<U>,
    {
        Eip8130Receipt { inner: f(self.inner), phase_statuses: self.phase_statuses }
    }

    /// Converts the receipt's log type by applying a function to each log,
    /// preserving the per-phase statuses.
    pub fn map_logs<U>(self, f: impl FnMut(T) -> U) -> Eip8130Receipt<U> {
        self.map_inner(|r| r.map_logs(f))
    }
}

impl<T> AsRef<Receipt<T>> for Eip8130Receipt<T> {
    fn as_ref(&self) -> &Receipt<T> {
        &self.inner
    }
}

impl<T> From<Eip8130Receipt<T>> for Receipt<T> {
    fn from(value: Eip8130Receipt<T>) -> Self {
        value.into_inner()
    }
}

impl<T> InMemorySize for Eip8130Receipt<T>
where
    Receipt<T>: InMemorySize,
{
    fn size(&self) -> usize {
        self.inner.size() + self.phase_statuses.capacity()
    }
}

#[cfg(feature = "arbitrary")]
impl<'a, T> arbitrary::Arbitrary<'a> for Eip8130Receipt<T>
where
    T: arbitrary::Arbitrary<'a>,
{
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self {
            inner: Receipt {
                status: alloy_consensus::Eip658Value::arbitrary(u)?,
                cumulative_gas_used: u64::arbitrary(u)?,
                logs: Vec::<T>::arbitrary(u)?,
            },
            phase_statuses: Vec::<u8>::arbitrary(u)?,
        })
    }
}
