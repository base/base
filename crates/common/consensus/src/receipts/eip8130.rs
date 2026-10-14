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

use alloy_consensus::{
    InMemorySize, Receipt, ReceiptWithBloom, RlpDecodableReceipt, RlpEncodableReceipt,
};
use alloy_primitives::Log;
use alloy_rlp::{BufMut, Decodable, Encodable};

/// Reason an [`Eip8130Receipt`]'s `phase_statuses` violate the receipt invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Eip8130PhaseStatusesError {
    /// The number of statuses differs from the transaction's number of call phases.
    #[error("{actual} phase statuses for a transaction with {expected} call phases")]
    CountMismatch {
        /// Number of call phases in the transaction.
        expected: usize,
        /// Number of statuses on the receipt.
        actual: usize,
    },
    /// A status byte is neither committed (`0x01`) nor reverted (`0x00`).
    #[error("phase {index} has unknown status {value:#04x}")]
    UnknownStatus {
        /// Index of the offending phase.
        index: usize,
        /// The status byte found.
        value: u8,
    },
    /// A phase is reported committed after an earlier phase reverted, but phases
    /// after a revert are skipped.
    #[error("phase {index} is reported committed after an earlier phase reverted")]
    CommittedAfterRevert {
        /// Index of the offending phase.
        index: usize,
    },
    /// The receipt status disagrees with the phase statuses: a successful
    /// receipt has every phase committed, a reverted one has a reverted phase.
    #[error("receipt status {status} disagrees with the phase statuses")]
    StatusMismatch {
        /// The receipt's overall status.
        status: bool,
    },
}

/// EIP-8130 account-abstraction receipt: a standard [`Receipt`] plus the
/// per-phase execution statuses.
///
/// Each entry of `phase_statuses` is `0x01` (the phase committed) or `0x00` (the
/// phase reverted, or was skipped because an earlier phase reverted). It is empty
/// when the transaction carried no `calls`. The overall [`Receipt::status`]
/// reports `true` only when every phase succeeded (or `calls` was empty),
/// matching the EIP-8130 receipt `status` semantics.
///
/// # Invariant
///
/// Non-empty `phase_statuses` hold exactly one entry per call phase of the
/// transaction, in phase order, shaped as zero or more `0x01` followed by zero or
/// more `0x00`, with [`Receipt::status`] `true` iff no entry is `0x00`. Empty
/// `phase_statuses` are always accepted: besides a transaction with no `calls`,
/// they mean the statuses are unavailable, since they are not part of the
/// consensus encoding (a receipt decoded from RLP, or one stored before the
/// field was persisted, carries none). [`Self::validate_phase_statuses`] checks
/// the invariant; it is enforced where receipts are served over RPC.
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
    /// Status byte recorded for a phase that committed.
    pub const PHASE_COMMITTED: u8 = 0x01;

    /// Status byte recorded for a phase that reverted or was skipped.
    pub const PHASE_REVERTED: u8 = 0x00;

    /// Checks `phase_statuses` against the [invariant](Self#invariant) for a
    /// transaction with `phase_count` call phases.
    pub fn validate_phase_statuses(
        &self,
        phase_count: usize,
    ) -> Result<(), Eip8130PhaseStatusesError> {
        if self.phase_statuses.is_empty() {
            return Ok(());
        }
        if self.phase_statuses.len() != phase_count {
            return Err(Eip8130PhaseStatusesError::CountMismatch {
                expected: phase_count,
                actual: self.phase_statuses.len(),
            });
        }
        let mut reverted = false;
        for (index, &value) in self.phase_statuses.iter().enumerate() {
            match value {
                Self::PHASE_COMMITTED if reverted => {
                    return Err(Eip8130PhaseStatusesError::CommittedAfterRevert { index });
                }
                Self::PHASE_COMMITTED => {}
                Self::PHASE_REVERTED => reverted = true,
                value => return Err(Eip8130PhaseStatusesError::UnknownStatus { index, value }),
            }
        }
        let status = self.inner.status.coerce_status();
        if status == reverted {
            return Err(Eip8130PhaseStatusesError::StatusMismatch { status });
        }
        Ok(())
    }

    /// Returns whether the phase at `index` committed. Phases past the end of
    /// `phase_statuses` did not run and report `false`.
    pub fn phase_committed(&self, index: usize) -> bool {
        self.phase_statuses.get(index) == Some(&Self::PHASE_COMMITTED)
    }

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

impl<T: Encodable> RlpEncodableReceipt for Eip8130Receipt<T> {
    fn rlp_encoded_length_with_bloom(&self, bloom: &alloy_primitives::Bloom) -> usize {
        self.inner.rlp_encoded_length_with_bloom(bloom)
    }

    fn rlp_encode_with_bloom(&self, bloom: &alloy_primitives::Bloom, out: &mut dyn BufMut) {
        self.inner.rlp_encode_with_bloom(bloom, out);
    }
}

impl<T: Decodable> RlpDecodableReceipt for Eip8130Receipt<T> {
    fn rlp_decode_with_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let ReceiptWithBloom { receipt, logs_bloom } = Receipt::rlp_decode_with_bloom(buf)?;
        Ok(ReceiptWithBloom { receipt: Self::new(receipt, Vec::new()), logs_bloom })
    }
}

impl<T> Encodable for Eip8130Receipt<T>
where
    Receipt<T>: Encodable,
{
    fn encode(&self, out: &mut dyn BufMut) {
        self.inner.encode(out);
    }

    fn length(&self) -> usize {
        self.inner.length()
    }
}

impl<T> Decodable for Eip8130Receipt<T>
where
    Receipt<T>: Decodable,
{
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self::new(Receipt::decode(buf)?, Vec::new()))
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

#[cfg(test)]
mod tests {
    use alloc::vec;

    use rstest::rstest;

    use super::*;

    fn receipt(status: bool, phase_statuses: Vec<u8>) -> Eip8130Receipt {
        Eip8130Receipt::new(
            Receipt { status: status.into(), cumulative_gas_used: 21_000, logs: vec![] },
            phase_statuses,
        )
    }

    #[rstest]
    #[case::no_calls(true, vec![], 0)]
    #[case::unavailable_on_success(true, vec![], 2)]
    #[case::unavailable_on_revert(false, vec![], 2)]
    #[case::all_committed(true, vec![0x01, 0x01], 2)]
    #[case::later_phase_reverted(false, vec![0x01, 0x00, 0x00], 3)]
    #[case::first_phase_reverted(false, vec![0x00, 0x00], 2)]
    fn accepts_well_formed_phase_statuses(
        #[case] status: bool,
        #[case] phase_statuses: Vec<u8>,
        #[case] phase_count: usize,
    ) {
        assert_eq!(receipt(status, phase_statuses).validate_phase_statuses(phase_count), Ok(()));
    }

    #[rstest]
    #[case::too_few(
        true,
        vec![0x01],
        2,
        Eip8130PhaseStatusesError::CountMismatch { expected: 2, actual: 1 },
    )]
    #[case::too_many(
        true,
        vec![0x01, 0x01, 0x01],
        2,
        Eip8130PhaseStatusesError::CountMismatch { expected: 2, actual: 3 },
    )]
    #[case::unknown_status(
        false,
        vec![0x01, 0xff],
        2,
        Eip8130PhaseStatusesError::UnknownStatus { index: 1, value: 0xff },
    )]
    #[case::committed_after_revert(
        false,
        vec![0x00, 0x01],
        2,
        Eip8130PhaseStatusesError::CommittedAfterRevert { index: 1 },
    )]
    #[case::success_with_reverted_phase(
        true,
        vec![0x01, 0x00],
        2,
        Eip8130PhaseStatusesError::StatusMismatch { status: true },
    )]
    #[case::revert_with_every_phase_committed(
        false,
        vec![0x01, 0x01],
        2,
        Eip8130PhaseStatusesError::StatusMismatch { status: false },
    )]
    fn rejects_malformed_phase_statuses(
        #[case] status: bool,
        #[case] phase_statuses: Vec<u8>,
        #[case] phase_count: usize,
        #[case] expected: Eip8130PhaseStatusesError,
    ) {
        assert_eq!(
            receipt(status, phase_statuses).validate_phase_statuses(phase_count),
            Err(expected)
        );
    }
}
