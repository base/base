use super::OpTxReceipt;
use alloy_primitives::{Bloom, Log};
use alloy_rlp::{length_of_length, BufMut, Decodable, Encodable};

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

/// Receipt containing result of transaction execution.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OpReceipt<T = Log> {
    /// If transaction is executed successfully.
    ///
    /// This is the `statusCode`
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity_bool"))]
    pub status: bool,
    /// Gas used
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::u128_hex_or_decimal"))]
    pub cumulative_gas_used: u128,
    /// Log send from contracts.
    pub logs: Vec<T>,
    /// Deposit nonce for Optimism deposit transactions
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub deposit_nonce: Option<u64>,
    /// Deposit receipt version for Optimism deposit transactions
    ///
    ///
    /// The deposit receipt version was introduced in Canyon to indicate an update to how
    /// receipt hashes should be computed when set. The state transition process
    /// ensures this is only set for post-Canyon deposit transactions.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub deposit_receipt_version: Option<u64>,
}

impl OpReceipt {
    /// Calculates [`Log`]'s bloom filter. this is slow operation and [OpReceiptWithBloom] can
    /// be used to cache this value.
    pub fn bloom_slow(&self) -> Bloom {
        self.logs.iter().collect()
    }

    /// Calculates the bloom filter for the receipt and returns the [OpReceiptWithBloom] container
    /// type.
    pub fn with_bloom(self) -> OpReceiptWithBloom {
        self.into()
    }
}

impl OpTxReceipt for OpReceipt {
    fn success(&self) -> bool {
        self.status
    }

    fn bloom(&self) -> Bloom {
        self.bloom_slow()
    }

    fn cumulative_gas_used(&self) -> u128 {
        self.cumulative_gas_used
    }

    fn logs(&self) -> &[Log] {
        &self.logs
    }

    fn deposit_nonce(&self) -> Option<u64> {
        self.deposit_nonce
    }

    fn deposit_receipt_version(&self) -> Option<u64> {
        self.deposit_receipt_version
    }
}

/// [`OpReceipt`] with calculated bloom filter, modified for the OP Stack.
///
/// This convenience type allows us to lazily calculate the bloom filter for a
/// receipt, similar to [`Sealed`].
///
/// [`Sealed`]: alloy_consensus::Sealed
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OpReceiptWithBloom<T = Log> {
    #[cfg_attr(feature = "serde", serde(flatten))]
    /// The receipt.
    pub receipt: OpReceipt<T>,
    /// The bloom filter.
    pub logs_bloom: Bloom,
}

impl OpTxReceipt for OpReceiptWithBloom {
    fn success(&self) -> bool {
        self.receipt.status
    }

    fn bloom(&self) -> Bloom {
        self.logs_bloom
    }

    fn bloom_cheap(&self) -> Option<Bloom> {
        Some(self.logs_bloom)
    }

    fn cumulative_gas_used(&self) -> u128 {
        self.receipt.cumulative_gas_used
    }

    fn logs(&self) -> &[Log] {
        &self.receipt.logs
    }

    fn deposit_nonce(&self) -> Option<u64> {
        self.receipt.deposit_nonce
    }

    fn deposit_receipt_version(&self) -> Option<u64> {
        self.receipt.deposit_receipt_version
    }
}

impl From<OpReceipt> for OpReceiptWithBloom {
    fn from(receipt: OpReceipt) -> Self {
        let bloom = receipt.bloom_slow();
        OpReceiptWithBloom { receipt, logs_bloom: bloom }
    }
}

impl OpReceiptWithBloom {
    /// Create new [OpReceiptWithBloom]
    pub const fn new(receipt: OpReceipt, bloom: Bloom) -> Self {
        Self { receipt, logs_bloom: bloom }
    }

    /// Consume the structure, returning only the receipt
    #[allow(clippy::missing_const_for_fn)] // false positive
    pub fn into_receipt(self) -> OpReceipt {
        self.receipt
    }

    /// Consume the structure, returning the receipt and the bloom filter
    #[allow(clippy::missing_const_for_fn)] // false positive
    pub fn into_components(self) -> (OpReceipt, Bloom) {
        (self.receipt, self.logs_bloom)
    }

    fn payload_len(&self) -> usize {
        self.receipt.status.length()
            + self.receipt.cumulative_gas_used.length()
            + self.logs_bloom.length()
            + self.receipt.logs.length()
            + self.receipt.deposit_nonce.map_or(0, |nonce| nonce.length())
            + self.receipt.deposit_receipt_version.map_or(0, |version| version.length())
    }

    /// Returns the rlp header for the receipt payload.
    fn receipt_rlp_header(&self) -> alloy_rlp::Header {
        alloy_rlp::Header { list: true, payload_length: self.payload_len() }
    }

    /// Encodes the receipt data.
    fn encode_fields(&self, out: &mut dyn BufMut) {
        self.receipt_rlp_header().encode(out);
        self.receipt.status.encode(out);
        self.receipt.cumulative_gas_used.encode(out);
        self.logs_bloom.encode(out);
        self.receipt.logs.encode(out);
        if let Some(nonce) = self.receipt.deposit_nonce {
            nonce.encode(out);
        }
        if let Some(version) = self.receipt.deposit_receipt_version {
            version.encode(out);
        }
    }

    /// Decodes the receipt payload
    fn decode_receipt(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let b: &mut &[u8] = &mut &**buf;
        let rlp_head = alloy_rlp::Header::decode(b)?;
        if !rlp_head.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let started_len = b.len();

        let success = Decodable::decode(b)?;
        let cumulative_gas_used = Decodable::decode(b)?;
        let bloom = Decodable::decode(b)?;
        let logs = Decodable::decode(b)?;

        let remaining = |b: &[u8]| rlp_head.payload_length - (started_len - b.len()) > 0;
        let deposit_nonce = remaining(b).then(|| alloy_rlp::Decodable::decode(b)).transpose()?;
        let deposit_receipt_version =
            remaining(b).then(|| alloy_rlp::Decodable::decode(b)).transpose()?;

        let receipt = OpReceipt {
            status: success,
            cumulative_gas_used,
            logs,
            deposit_nonce,
            deposit_receipt_version,
        };

        let this = Self { receipt, logs_bloom: bloom };
        let consumed = started_len - b.len();
        if consumed != rlp_head.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: rlp_head.payload_length,
                got: consumed,
            });
        }
        *buf = *b;
        Ok(this)
    }
}

impl alloy_rlp::Encodable for OpReceiptWithBloom {
    fn encode(&self, out: &mut dyn BufMut) {
        self.encode_fields(out);
    }

    fn length(&self) -> usize {
        let payload_length = self.receipt.status.length()
            + self.receipt.cumulative_gas_used.length()
            + self.logs_bloom.length()
            + self.receipt.logs.length()
            + self.receipt.deposit_nonce.map_or(0, |nonce| nonce.length())
            + self.receipt.deposit_receipt_version.map_or(0, |version| version.length());
        payload_length + length_of_length(payload_length)
    }
}

impl alloy_rlp::Decodable for OpReceiptWithBloom {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::decode_receipt(buf)
    }
}

#[cfg(all(test, feature = "arbitrary"))]
impl<'a, T> arbitrary::Arbitrary<'a> for OpReceipt<T>
where
    T: arbitrary::Arbitrary<'a>,
{
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let deposit_nonce = Option::<u64>::arbitrary(u)?;
        let deposit_receipt_version =
            deposit_nonce.is_some().then(|| u64::arbitrary(u)).transpose()?;
        Ok(Self {
            status: bool::arbitrary(u)?,
            cumulative_gas_used: u128::arbitrary(u)?,
            logs: Vec::<T>::arbitrary(u)?,
            deposit_nonce,
            deposit_receipt_version,
        })
    }
}

#[cfg(all(test, feature = "arbitrary"))]
impl<'a, T> arbitrary::Arbitrary<'a> for OpReceiptWithBloom<T>
where
    T: arbitrary::Arbitrary<'a>,
{
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self { receipt: OpReceipt::<T>::arbitrary(u)?, logs_bloom: Bloom::arbitrary(u)? })
    }
}
