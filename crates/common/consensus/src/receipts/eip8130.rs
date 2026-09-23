//! EIP-8130 (account-abstraction) receipt type for Base chains.
//!
//! [`Eip8130Receipt`] is a standard [`Receipt`] augmented with the resolved gas
//! payer and the per-phase execution statuses. Both are part of the consensus
//! receipt:
//!
//! ```text
//! rlp([status, cumulative_gas_used, logs_bloom, logs, payer, phase_statuses])
//! ```
//!
//! `payer` is stored because in open payer mode it can only be derived from the
//! transaction by recovering `payer_auth`. `phase_statuses` is one byte per
//! phase, encoded as an RLP byte string.

use alloc::vec::Vec;

use alloy_consensus::{
    Eip658Value, InMemorySize, Receipt, ReceiptWithBloom, RlpDecodableReceipt, RlpEncodableReceipt,
};
use alloy_primitives::{Address, Bloom, Bytes, Log};
use alloy_rlp::{BufMut, Decodable, Encodable, Header};

/// Values of an [`Eip8130Receipt::phase_statuses`] entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130PhaseStatus;

impl Eip8130PhaseStatus {
    /// The phase reverted.
    pub const REVERTED: u8 = 0x00;
    /// The phase committed.
    pub const SUCCEEDED: u8 = 0x01;
    /// The phase was skipped because an earlier phase reverted.
    pub const SKIPPED: u8 = 0x02;
}

/// EIP-8130 account-abstraction receipt: a standard [`Receipt`] plus the
/// resolved gas payer and the per-phase execution statuses.
///
/// Each entry of `phase_statuses` is an [`Eip8130PhaseStatus`] value. It is empty when the transaction carried no `calls`. The
/// overall [`Receipt::status`] reports `true` only when every phase succeeded
/// (or `calls` was empty).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Eip8130Receipt<T = Log> {
    /// The inner (standard) receipt: status, cumulative gas used, and logs.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub inner: Receipt<T>,
    /// The account that paid gas: the sender for self-pay, the named payer, or
    /// the recovered signer in open payer mode.
    ///
    /// Skipped by serde: the RPC receipt surfaces it as its own `payer` field.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub payer: Address,
    /// Per-phase execution statuses.
    ///
    /// Skipped by serde: the RPC receipt surfaces it as its own `phaseStatuses`
    /// field.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub phase_statuses: Vec<u8>,
}

impl<T> Eip8130Receipt<T> {
    /// Creates a new [`Eip8130Receipt`].
    pub const fn new(inner: Receipt<T>, payer: Address, phase_statuses: Vec<u8>) -> Self {
        Self { inner, payer, phase_statuses }
    }

    /// Consumes the type and returns the inner [`Receipt`].
    pub fn into_inner(self) -> Receipt<T> {
        self.inner
    }

    /// Maps the inner receipt, preserving the payer and per-phase statuses.
    pub fn map_inner<U, F>(self, f: F) -> Eip8130Receipt<U>
    where
        F: FnOnce(Receipt<T>) -> Receipt<U>,
    {
        Eip8130Receipt {
            inner: f(self.inner),
            payer: self.payer,
            phase_statuses: self.phase_statuses,
        }
    }

    /// Converts the receipt's log type by applying a function to each log,
    /// preserving the payer and per-phase statuses.
    pub fn map_logs<U>(self, f: impl FnMut(T) -> U) -> Eip8130Receipt<U> {
        self.map_inner(|r| r.map_logs(f))
    }
}

impl<T: Encodable> Eip8130Receipt<T> {
    /// Length of the RLP-encoded fields with `bloom`, without a list header.
    pub fn rlp_encoded_fields_length_with_bloom(&self, bloom: &Bloom) -> usize {
        self.inner.rlp_encoded_fields_length_with_bloom(bloom)
            + self.payer.length()
            + self.phase_statuses.as_slice().length()
    }

    /// RLP-encodes the fields with `bloom`, without a list header.
    pub fn rlp_encode_fields_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        self.inner.rlp_encode_fields_with_bloom(bloom, out);
        self.payer.encode(out);
        self.phase_statuses.as_slice().encode(out);
    }

    /// Length of the RLP-encoded fields without the bloom and list header:
    /// `status, cumulative_gas_used, logs, payer, phase_statuses`.
    pub fn rlp_encoded_fields_length_without_bloom(&self) -> usize {
        self.inner.status.length()
            + self.inner.cumulative_gas_used.length()
            + self.inner.logs.length()
            + self.payer.length()
            + self.phase_statuses.as_slice().length()
    }

    /// RLP-encodes the fields without the bloom and list header.
    pub fn rlp_encode_fields_without_bloom(&self, out: &mut dyn BufMut) {
        self.inner.status.encode(out);
        self.inner.cumulative_gas_used.encode(out);
        self.inner.logs.encode(out);
        self.payer.encode(out);
        self.phase_statuses.as_slice().encode(out);
    }
}

impl<T: Decodable> Eip8130Receipt<T> {
    /// Decodes the trailing `payer, phase_statuses` fields.
    pub fn rlp_decode_trailing_fields(buf: &mut &[u8]) -> alloy_rlp::Result<(Address, Vec<u8>)> {
        let payer = Decodable::decode(buf)?;
        let phase_statuses = Bytes::decode(buf)?.to_vec();
        Ok((payer, phase_statuses))
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
        let payload_length = self.rlp_encoded_fields_length_with_bloom(bloom);
        Header { list: true, payload_length }.length_with_payload()
    }

    fn rlp_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        Header { list: true, payload_length: self.rlp_encoded_fields_length_with_bloom(bloom) }
            .encode(out);
        self.rlp_encode_fields_with_bloom(bloom, out);
    }
}

impl<T: Decodable> RlpDecodableReceipt for Eip8130Receipt<T> {
    fn rlp_decode_with_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let remaining = buf.len();
        let status: Eip658Value = Decodable::decode(buf)?;
        let cumulative_gas_used = Decodable::decode(buf)?;
        let logs_bloom = Decodable::decode(buf)?;
        let logs = Decodable::decode(buf)?;
        let (payer, phase_statuses) = Self::rlp_decode_trailing_fields(buf)?;
        if remaining - buf.len() != header.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: remaining - buf.len(),
            });
        }
        Ok(ReceiptWithBloom {
            receipt: Self::new(
                Receipt { status, cumulative_gas_used, logs },
                payer,
                phase_statuses,
            ),
            logs_bloom,
        })
    }
}

impl<T: Encodable> Encodable for Eip8130Receipt<T> {
    fn encode(&self, out: &mut dyn BufMut) {
        Header { list: true, payload_length: self.rlp_encoded_fields_length_without_bloom() }
            .encode(out);
        self.rlp_encode_fields_without_bloom(out);
    }

    fn length(&self) -> usize {
        let payload_length = self.rlp_encoded_fields_length_without_bloom();
        Header { list: true, payload_length }.length_with_payload()
    }
}

impl<T: Decodable> Decodable for Eip8130Receipt<T> {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let remaining = buf.len();
        let status = Decodable::decode(buf)?;
        let cumulative_gas_used = Decodable::decode(buf)?;
        let logs = Decodable::decode(buf)?;
        let (payer, phase_statuses) = Self::rlp_decode_trailing_fields(buf)?;
        if remaining - buf.len() != header.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: header.payload_length,
                got: remaining - buf.len(),
            });
        }
        Ok(Self::new(Receipt { status, cumulative_gas_used, logs }, payer, phase_statuses))
    }
}

impl<T> InMemorySize for Eip8130Receipt<T>
where
    Receipt<T>: InMemorySize,
{
    fn size(&self) -> usize {
        self.inner.size() + core::mem::size_of::<Address>() + self.phase_statuses.capacity()
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
                status: Eip658Value::arbitrary(u)?,
                cumulative_gas_used: u64::arbitrary(u)?,
                logs: Vec::<T>::arbitrary(u)?,
            },
            payer: Address::arbitrary(u)?,
            phase_statuses: Vec::<u8>::arbitrary(u)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, bytes};

    use super::*;

    fn sample() -> Eip8130Receipt {
        Eip8130Receipt::new(
            Receipt {
                status: Eip658Value::Eip658(false),
                cumulative_gas_used: 21_000,
                logs: vec![Log::new_unchecked(
                    address!("0x00000000000000000000000000000000000000aa"),
                    vec![],
                    bytes!("01"),
                )],
            },
            address!("0x00000000000000000000000000000000000000bb"),
            vec![
                Eip8130PhaseStatus::SUCCEEDED,
                Eip8130PhaseStatus::REVERTED,
                Eip8130PhaseStatus::SKIPPED,
            ],
        )
    }

    #[test]
    fn rlp_with_bloom_roundtrips_payer_and_phase_statuses() {
        let receipt = sample();
        let bloom = Bloom::repeat_byte(0x11);
        let mut buf = Vec::new();
        receipt.rlp_encode_with_bloom(&bloom, &mut buf);
        assert_eq!(buf.len(), receipt.rlp_encoded_length_with_bloom(&bloom));

        let decoded = Eip8130Receipt::<Log>::rlp_decode_with_bloom(&mut buf.as_slice()).unwrap();
        assert_eq!(decoded.receipt, receipt);
        assert_eq!(decoded.logs_bloom, bloom);
    }

    #[test]
    fn rlp_with_bloom_appends_payer_and_statuses_after_logs() {
        let receipt = sample();
        let bloom = Bloom::ZERO;
        let mut encoded = Vec::new();
        receipt.rlp_encode_fields_with_bloom(&bloom, &mut encoded);

        let mut standard = Vec::new();
        receipt.inner.rlp_encode_fields_with_bloom(&bloom, &mut standard);
        let mut tail = Vec::new();
        receipt.payer.encode(&mut tail);
        Bytes::from(receipt.phase_statuses.clone()).encode(&mut tail);

        assert_eq!(encoded, [standard, tail].concat());
    }

    #[test]
    fn rlp_without_bloom_roundtrips() {
        let receipt = sample();
        let mut buf = Vec::new();
        receipt.encode(&mut buf);
        assert_eq!(buf.len(), receipt.length());
        assert_eq!(Eip8130Receipt::<Log>::decode(&mut buf.as_slice()).unwrap(), receipt);
    }
}
