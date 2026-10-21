//! EIP-8130 (account-abstraction) receipt type for Base chains.
//!
//! [`Eip8130Receipt`] is a standard [`Receipt`] plus `committedPhases`, the
//! number of leading call phases that committed. The count is part of the
//! consensus receipt: it is the last field of the RLP encoding, so it is covered
//! by the receipts root and by receipt proofs.

use alloc::vec::Vec;

use alloy_consensus::{
    InMemorySize, Receipt, ReceiptWithBloom, RlpDecodableReceipt, RlpEncodableReceipt,
};
use alloy_primitives::{Bloom, Log};
use alloy_rlp::{Buf, BufMut, Decodable, Encodable, Header};

/// Reason an [`Eip8130Receipt`]'s `committed_phases` violates the receipt invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Eip8130CommittedPhasesError {
    /// More phases are reported committed than the transaction has.
    #[error("{committed} committed phases for a transaction with {phase_count} call phases")]
    ExceedsPhaseCount {
        /// The receipt's committed phase count.
        committed: u64,
        /// Number of call phases in the transaction.
        phase_count: usize,
    },
    /// The receipt status disagrees with the committed phase count: a successful
    /// receipt has every phase committed, a reverted one has fewer.
    #[error("receipt status {status} disagrees with the committed phase count")]
    StatusMismatch {
        /// The receipt's overall status.
        status: bool,
    },
}

/// EIP-8130 account-abstraction receipt: a standard [`Receipt`] plus the number
/// of call phases that committed.
///
/// Phases run in order and stop at the first revert, so the outcome of every
/// phase follows from one count. When `committed_phases` is less than the
/// transaction's phase count, phase `committed_phases` (zero-based) reverted and
/// every later phase was skipped. The overall [`Receipt::status`] is `true` only
/// when every phase committed (or `calls` was empty), matching the EIP-8130
/// receipt `status` semantics. A reverted receipt can still carry the state
/// changes and logs of its committed phases.
///
/// # Invariant
///
/// For a transaction with `phase_count` call phases, `committed_phases <=
/// phase_count`, and [`Receipt::status`] is `true` iff `committed_phases ==
/// phase_count`. [`Self::validate_committed_phases`] checks the invariant.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Eip8130Receipt<T = Log> {
    /// The inner (standard) receipt: status, cumulative gas used, and logs.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub inner: Receipt<T>,
    /// Number of leading call phases that committed.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub committed_phases: u64,
}

impl<T> Eip8130Receipt<T> {
    /// Status byte reported for a phase that committed.
    pub const PHASE_COMMITTED: u8 = 0x01;

    /// Status byte reported for a phase that reverted or was skipped.
    pub const PHASE_REVERTED: u8 = 0x00;

    /// Checks `committed_phases` against the [invariant](Self#invariant) for a
    /// transaction with `phase_count` call phases.
    pub fn validate_committed_phases(
        &self,
        phase_count: usize,
    ) -> Result<(), Eip8130CommittedPhasesError> {
        let all_committed = match usize::try_from(self.committed_phases) {
            Ok(committed) if committed <= phase_count => committed == phase_count,
            _ => {
                return Err(Eip8130CommittedPhasesError::ExceedsPhaseCount {
                    committed: self.committed_phases,
                    phase_count,
                });
            }
        };
        let status = self.inner.status.coerce_status();
        if status != all_committed {
            return Err(Eip8130CommittedPhasesError::StatusMismatch { status });
        }
        Ok(())
    }

    /// Returns the per-phase statuses for a transaction with `phase_count` call
    /// phases: [`Self::PHASE_COMMITTED`] for each committed phase, then
    /// [`Self::PHASE_REVERTED`] for the reverted phase and every skipped one.
    pub fn phase_statuses(
        &self,
        phase_count: usize,
    ) -> Result<Vec<u8>, Eip8130CommittedPhasesError> {
        self.validate_committed_phases(phase_count)?;
        let committed = self.committed_phases as usize;
        let mut statuses = Vec::with_capacity(phase_count);
        statuses.resize(committed, Self::PHASE_COMMITTED);
        statuses.resize(phase_count, Self::PHASE_REVERTED);
        Ok(statuses)
    }

    /// Returns whether the phase at `index` committed.
    pub fn phase_committed(&self, index: usize) -> bool {
        u64::try_from(index).is_ok_and(|index| index < self.committed_phases)
    }

    /// Creates a new [`Eip8130Receipt`] from an inner receipt and its committed
    /// phase count.
    pub const fn new(inner: Receipt<T>, committed_phases: u64) -> Self {
        Self { inner, committed_phases }
    }

    /// Consumes the type and returns the inner [`Receipt`].
    pub fn into_inner(self) -> Receipt<T> {
        self.inner
    }

    /// Maps the inner receipt, preserving the committed phase count.
    pub fn map_inner<U, F>(self, f: F) -> Eip8130Receipt<U>
    where
        F: FnOnce(Receipt<T>) -> Receipt<U>,
    {
        Eip8130Receipt { inner: f(self.inner), committed_phases: self.committed_phases }
    }

    /// Converts the receipt's log type by applying a function to each log,
    /// preserving the committed phase count.
    pub fn map_logs<U>(self, f: impl FnMut(T) -> U) -> Eip8130Receipt<U> {
        self.map_inner(|r| r.map_logs(f))
    }
}

impl<T: Encodable> Eip8130Receipt<T> {
    /// Returns length of RLP-encoded receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encoded_fields_length_with_bloom(&self, bloom: &Bloom) -> usize {
        self.inner.rlp_encoded_fields_length_with_bloom(bloom) + self.committed_phases.length()
    }

    /// RLP-encodes receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encode_fields_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        self.inner.rlp_encode_fields_with_bloom(bloom, out);
        self.committed_phases.encode(out);
    }

    /// Returns RLP header for this receipt encoding with the given [`Bloom`].
    pub fn rlp_header_with_bloom(&self, bloom: &Bloom) -> Header {
        Header { list: true, payload_length: self.rlp_encoded_fields_length_with_bloom(bloom) }
    }

    /// Returns length of RLP-encoded receipt fields without the bloom and without an RLP header.
    pub fn rlp_encoded_fields_length_without_bloom(&self) -> usize {
        self.inner.status.length()
            + self.inner.cumulative_gas_used.length()
            + self.inner.logs.length()
            + self.committed_phases.length()
    }

    /// RLP-encodes receipt fields without the bloom and without an RLP header.
    pub fn rlp_encode_fields_without_bloom(&self, out: &mut dyn BufMut) {
        self.inner.status.encode(out);
        self.inner.cumulative_gas_used.encode(out);
        self.inner.logs.encode(out);
        self.committed_phases.encode(out);
    }
}

impl<T: Decodable> Eip8130Receipt<T> {
    /// RLP-decodes receipt fields with a [`Bloom`].
    ///
    /// Does not expect an RLP header.
    pub fn rlp_decode_fields_with_bloom(
        buf: &mut &[u8],
    ) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let ReceiptWithBloom { receipt: inner, logs_bloom } =
            Receipt::rlp_decode_fields_with_bloom(buf)?;
        let committed_phases = Decodable::decode(buf)?;
        Ok(ReceiptWithBloom { receipt: Self { inner, committed_phases }, logs_bloom })
    }

    /// RLP-decodes receipt fields without the bloom.
    ///
    /// Does not expect an RLP header.
    pub fn rlp_decode_fields_without_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let status = Decodable::decode(buf)?;
        let cumulative_gas_used = Decodable::decode(buf)?;
        let logs = Decodable::decode(buf)?;
        let committed_phases = Decodable::decode(buf)?;
        Ok(Self { inner: Receipt { status, cumulative_gas_used, logs }, committed_phases })
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
    fn rlp_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        self.rlp_header_with_bloom(bloom).length_with_payload()
    }

    fn rlp_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        self.rlp_header_with_bloom(bloom).encode(out);
        self.rlp_encode_fields_with_bloom(bloom, out);
    }
}

impl<T: Decodable> RlpDecodableReceipt for Eip8130Receipt<T> {
    fn rlp_decode_with_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        if buf.len() < header.payload_length {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let mut fields_buf = &buf[..header.payload_length];
        let this = Self::rlp_decode_fields_with_bloom(&mut fields_buf)?;
        if !fields_buf.is_empty() {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        buf.advance(header.payload_length);
        Ok(this)
    }
}

impl<T: Encodable> Encodable for Eip8130Receipt<T> {
    fn encode(&self, out: &mut dyn BufMut) {
        Header { list: true, payload_length: self.rlp_encoded_fields_length_without_bloom() }
            .encode(out);
        self.rlp_encode_fields_without_bloom(out);
    }

    fn length(&self) -> usize {
        Header { list: true, payload_length: self.rlp_encoded_fields_length_without_bloom() }
            .length_with_payload()
    }
}

impl<T: Decodable> Decodable for Eip8130Receipt<T> {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        if buf.len() < header.payload_length {
            return Err(alloy_rlp::Error::InputTooShort);
        }

        let mut fields_buf = &buf[..header.payload_length];
        let this = Self::rlp_decode_fields_without_bloom(&mut fields_buf)?;
        if !fields_buf.is_empty() {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        buf.advance(header.payload_length);
        Ok(this)
    }
}

impl<T> InMemorySize for Eip8130Receipt<T>
where
    Receipt<T>: InMemorySize,
{
    fn size(&self) -> usize {
        self.inner.size() + core::mem::size_of_val(&self.committed_phases)
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
            committed_phases: u64::arbitrary(u)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use rstest::rstest;

    use super::*;

    fn receipt(status: bool, committed_phases: u64) -> Eip8130Receipt {
        Eip8130Receipt::new(
            Receipt { status: status.into(), cumulative_gas_used: 21_000, logs: vec![] },
            committed_phases,
        )
    }

    #[rstest]
    #[case::no_calls(true, 0, 0, vec![])]
    #[case::all_committed(true, 2, 2, vec![0x01, 0x01])]
    #[case::later_phase_reverted(false, 1, 3, vec![0x01, 0x00, 0x00])]
    #[case::first_phase_reverted(false, 0, 2, vec![0x00, 0x00])]
    fn phase_statuses_follow_the_committed_count(
        #[case] status: bool,
        #[case] committed_phases: u64,
        #[case] phase_count: usize,
        #[case] expected: Vec<u8>,
    ) {
        assert_eq!(receipt(status, committed_phases).phase_statuses(phase_count), Ok(expected));
    }

    #[rstest]
    #[case::exceeds_phase_count(
        true,
        3,
        2,
        Eip8130CommittedPhasesError::ExceedsPhaseCount { committed: 3, phase_count: 2 },
    )]
    #[case::success_with_reverted_phase(
        true,
        1,
        2,
        Eip8130CommittedPhasesError::StatusMismatch { status: true },
    )]
    #[case::revert_with_every_phase_committed(
        false,
        2,
        2,
        Eip8130CommittedPhasesError::StatusMismatch { status: false },
    )]
    fn rejects_inconsistent_committed_phases(
        #[case] status: bool,
        #[case] committed_phases: u64,
        #[case] phase_count: usize,
        #[case] expected: Eip8130CommittedPhasesError,
    ) {
        assert_eq!(
            receipt(status, committed_phases).validate_committed_phases(phase_count),
            Err(expected)
        );
    }

    #[test]
    fn phase_committed_reports_only_the_leading_phases() {
        let receipt = receipt(false, 1);
        assert!(receipt.phase_committed(0));
        assert!(!receipt.phase_committed(1));
        assert!(!receipt.phase_committed(usize::MAX));
    }

    #[test]
    fn rlp_with_bloom_roundtrips_the_committed_count() {
        let receipt = receipt(false, 1);
        let bloom = Bloom::default();
        let mut buf = Vec::new();
        receipt.rlp_encode_with_bloom(&bloom, &mut buf);
        assert_eq!(buf.len(), receipt.rlp_encoded_length_with_bloom(&bloom));

        let decoded = Eip8130Receipt::<Log>::rlp_decode_with_bloom(&mut buf.as_slice()).unwrap();
        assert_eq!(decoded.receipt, receipt);
    }

    #[test]
    fn rlp_rejects_a_missing_committed_count() {
        let inner: Receipt<Log> =
            Receipt { status: true.into(), cumulative_gas_used: 21_000, logs: vec![] };
        let mut buf = Vec::new();
        inner.rlp_encode_with_bloom(&Bloom::default(), &mut buf);

        assert!(Eip8130Receipt::<Log>::rlp_decode_with_bloom(&mut buf.as_slice()).is_err());
    }
}
