//! Receipt type for execution and storage.

use alloc::vec::Vec;
use core::fmt::Debug;

use alloy_consensus::{
    Eip658Value, Eip2718DecodableReceipt, Eip2718EncodableReceipt, InMemorySize, Receipt,
    ReceiptWithBloom, RlpDecodableReceipt, RlpEncodableReceipt, TxReceipt, Typed2718,
};
use alloy_eips::eip2718::{Eip2718Error, Eip2718Result, IsTyped2718};
use alloy_primitives::{Bloom, Log};
use alloy_rlp::{Buf, BufMut, Decodable, Encodable, Header};

use super::{BaseTxReceipt, DepositReceipt};
use crate::{BaseReceiptEnvelope, OpTxType};

/// Transaction receipt for Base chains.
///
/// Receipt containing result of transaction execution.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", serde(tag = "type"))]
pub enum BaseReceipt<T = Log> {
    /// Legacy receipt
    #[cfg_attr(feature = "serde", serde(rename = "0x0", alias = "0x00"))]
    Legacy(Receipt<T>),
    /// EIP-2930 receipt
    #[cfg_attr(feature = "serde", serde(rename = "0x1", alias = "0x01"))]
    Eip2930(Receipt<T>),
    /// EIP-1559 receipt
    #[cfg_attr(feature = "serde", serde(rename = "0x2", alias = "0x02"))]
    Eip1559(Receipt<T>),
    /// EIP-7702 receipt
    #[cfg_attr(feature = "serde", serde(rename = "0x4", alias = "0x04"))]
    Eip7702(Receipt<T>),
    /// Deposit receipt
    #[cfg_attr(feature = "serde", serde(rename = "0x7e", alias = "0x7E"))]
    Deposit(DepositReceipt<T>),
}

impl<T> BaseReceipt<T> {
    /// Returns [`OpTxType`] of the receipt.
    pub const fn tx_type(&self) -> OpTxType {
        match self {
            Self::Legacy(_) => OpTxType::Legacy,
            Self::Eip2930(_) => OpTxType::Eip2930,
            Self::Eip1559(_) => OpTxType::Eip1559,
            Self::Eip7702(_) => OpTxType::Eip7702,
            Self::Deposit(_) => OpTxType::Deposit,
        }
    }

    /// Returns inner [`Receipt`].
    pub const fn as_receipt(&self) -> &Receipt<T> {
        match self {
            Self::Legacy(receipt)
            | Self::Eip2930(receipt)
            | Self::Eip1559(receipt)
            | Self::Eip7702(receipt) => receipt,
            Self::Deposit(receipt) => &receipt.inner,
        }
    }

    /// Returns a mutable reference to the inner [`Receipt`].
    pub const fn as_receipt_mut(&mut self) -> &mut Receipt<T> {
        match self {
            Self::Legacy(receipt)
            | Self::Eip2930(receipt)
            | Self::Eip1559(receipt)
            | Self::Eip7702(receipt) => receipt,
            Self::Deposit(receipt) => &mut receipt.inner,
        }
    }

    /// Consumes this and returns the inner [`Receipt`].
    pub fn into_receipt(self) -> Receipt<T> {
        match self {
            Self::Legacy(receipt)
            | Self::Eip2930(receipt)
            | Self::Eip1559(receipt)
            | Self::Eip7702(receipt) => receipt,
            Self::Deposit(receipt) => receipt.inner,
        }
    }

    /// Converts the receipt's log type by applying a function to each log.
    ///
    /// Returns the receipt with the new log type
    pub fn map_logs<U>(self, f: impl FnMut(T) -> U) -> BaseReceipt<U> {
        match self {
            Self::Legacy(receipt) => BaseReceipt::Legacy(receipt.map_logs(f)),
            Self::Eip2930(receipt) => BaseReceipt::Eip2930(receipt.map_logs(f)),
            Self::Eip1559(receipt) => BaseReceipt::Eip1559(receipt.map_logs(f)),
            Self::Eip7702(receipt) => BaseReceipt::Eip7702(receipt.map_logs(f)),
            Self::Deposit(receipt) => BaseReceipt::Deposit(receipt.map_logs(f)),
        }
    }

    /// Returns length of RLP-encoded receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encoded_fields_length(&self, bloom: &Bloom) -> usize
    where
        T: Encodable,
    {
        match self {
            Self::Legacy(receipt)
            | Self::Eip2930(receipt)
            | Self::Eip1559(receipt)
            | Self::Eip7702(receipt) => receipt.rlp_encoded_fields_length_with_bloom(bloom),
            Self::Deposit(receipt) => receipt.rlp_encoded_fields_length_with_bloom(bloom),
        }
    }

    /// RLP-encodes receipt fields with the given [`Bloom`] without an RLP header.
    pub fn rlp_encode_fields(&self, bloom: &Bloom, out: &mut dyn BufMut)
    where
        T: Encodable,
    {
        match self {
            Self::Legacy(receipt)
            | Self::Eip2930(receipt)
            | Self::Eip1559(receipt)
            | Self::Eip7702(receipt) => receipt.rlp_encode_fields_with_bloom(bloom, out),
            Self::Deposit(receipt) => receipt.rlp_encode_fields_with_bloom(bloom, out),
        }
    }

    /// Returns RLP header for inner encoding.
    pub fn rlp_header_inner(&self, bloom: &Bloom) -> Header
    where
        T: Encodable,
    {
        Header { list: true, payload_length: self.rlp_encoded_fields_length(bloom) }
    }

    /// Returns RLP header for inner encoding without bloom.
    pub fn rlp_header_without_bloom(&self) -> Header
    where
        T: Encodable,
    {
        Header { list: true, payload_length: self.rlp_encoded_fields_length_without_bloom() }
    }

    /// RLP-decodes the receipt from the provided buffer. This does not expect a type byte or
    /// network header.
    pub fn rlp_decode_inner(
        buf: &mut &[u8],
        tx_type: OpTxType,
    ) -> alloy_rlp::Result<ReceiptWithBloom<Self>>
    where
        T: Decodable,
    {
        match tx_type {
            OpTxType::Legacy => {
                let ReceiptWithBloom { receipt, logs_bloom } =
                    RlpDecodableReceipt::rlp_decode_with_bloom(buf)?;
                Ok(ReceiptWithBloom { receipt: Self::Legacy(receipt), logs_bloom })
            }
            OpTxType::Eip2930 => {
                let ReceiptWithBloom { receipt, logs_bloom } =
                    RlpDecodableReceipt::rlp_decode_with_bloom(buf)?;
                Ok(ReceiptWithBloom { receipt: Self::Eip2930(receipt), logs_bloom })
            }
            OpTxType::Eip1559 => {
                let ReceiptWithBloom { receipt, logs_bloom } =
                    RlpDecodableReceipt::rlp_decode_with_bloom(buf)?;
                Ok(ReceiptWithBloom { receipt: Self::Eip1559(receipt), logs_bloom })
            }
            OpTxType::Eip7702 => {
                let ReceiptWithBloom { receipt, logs_bloom } =
                    RlpDecodableReceipt::rlp_decode_with_bloom(buf)?;
                Ok(ReceiptWithBloom { receipt: Self::Eip7702(receipt), logs_bloom })
            }
            OpTxType::Deposit => {
                let ReceiptWithBloom { receipt, logs_bloom } =
                    RlpDecodableReceipt::rlp_decode_with_bloom(buf)?;
                Ok(ReceiptWithBloom { receipt: Self::Deposit(receipt), logs_bloom })
            }
        }
    }

    /// RLP-encodes receipt fields without an RLP header.
    pub fn rlp_encode_fields_without_bloom(&self, out: &mut dyn BufMut)
    where
        T: Encodable,
    {
        self.tx_type().encode(out);
        match self {
            Self::Legacy(receipt)
            | Self::Eip2930(receipt)
            | Self::Eip1559(receipt)
            | Self::Eip7702(receipt) => {
                receipt.status.encode(out);
                receipt.cumulative_gas_used.encode(out);
                receipt.logs.encode(out);
            }
            Self::Deposit(receipt) => {
                receipt.inner.status.encode(out);
                receipt.inner.cumulative_gas_used.encode(out);
                receipt.inner.logs.encode(out);
                if let Some(nonce) = receipt.deposit_nonce {
                    nonce.encode(out);
                }
                if let Some(version) = receipt.deposit_receipt_version {
                    version.encode(out);
                }
            }
        }
    }

    /// Returns length of RLP-encoded receipt fields without an RLP header.
    pub fn rlp_encoded_fields_length_without_bloom(&self) -> usize
    where
        T: Encodable,
    {
        self.tx_type().length()
            + match self {
                Self::Legacy(receipt)
                | Self::Eip2930(receipt)
                | Self::Eip1559(receipt)
                | Self::Eip7702(receipt) => {
                    receipt.status.length()
                        + receipt.cumulative_gas_used.length()
                        + receipt.logs.length()
                }
                Self::Deposit(receipt) => {
                    receipt.inner.status.length()
                        + receipt.inner.cumulative_gas_used.length()
                        + receipt.inner.logs.length()
                        + receipt.deposit_nonce.map_or(0, |nonce| nonce.length())
                        + receipt.deposit_receipt_version.map_or(0, |version| version.length())
                }
            }
    }

    /// RLP-decodes the receipt from the provided buffer without bloom.
    pub fn rlp_decode_fields_without_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<Self>
    where
        T: Decodable,
    {
        let tx_type = OpTxType::decode(buf)?;
        let status = Decodable::decode(buf)?;
        let cumulative_gas_used = Decodable::decode(buf)?;
        let logs = Decodable::decode(buf)?;

        let mut deposit_nonce = None;
        let mut deposit_receipt_version = None;

        // For deposit receipts, try to decode nonce and version if they exist
        if tx_type == OpTxType::Deposit && !buf.is_empty() {
            deposit_nonce = Some(Decodable::decode(buf)?);
            if !buf.is_empty() {
                deposit_receipt_version = Some(Decodable::decode(buf)?);
            }
        }

        match tx_type {
            OpTxType::Legacy => Ok(Self::Legacy(Receipt { status, cumulative_gas_used, logs })),
            OpTxType::Eip2930 => Ok(Self::Eip2930(Receipt { status, cumulative_gas_used, logs })),
            OpTxType::Eip1559 => Ok(Self::Eip1559(Receipt { status, cumulative_gas_used, logs })),
            OpTxType::Eip7702 => Ok(Self::Eip7702(Receipt { status, cumulative_gas_used, logs })),
            OpTxType::Deposit => Ok(Self::Deposit(DepositReceipt {
                inner: Receipt { status, cumulative_gas_used, logs },
                deposit_nonce,
                deposit_receipt_version,
            })),
        }
    }
}

impl<T: Encodable> Eip2718EncodableReceipt for BaseReceipt<T> {
    fn eip2718_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        !self.tx_type().is_legacy() as usize + self.rlp_header_inner(bloom).length_with_payload()
    }

    fn eip2718_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        if !self.tx_type().is_legacy() {
            out.put_u8(self.tx_type() as u8);
        }
        self.rlp_header_inner(bloom).encode(out);
        self.rlp_encode_fields(bloom, out);
    }
}

impl<T: Decodable> Eip2718DecodableReceipt for BaseReceipt<T> {
    fn typed_decode_with_bloom(ty: u8, buf: &mut &[u8]) -> Eip2718Result<ReceiptWithBloom<Self>> {
        let tx_type = OpTxType::try_from(ty).map_err(|_| Eip2718Error::UnexpectedType(ty))?;
        Ok(Self::rlp_decode_inner(buf, tx_type)?)
    }

    fn fallback_decode_with_bloom(buf: &mut &[u8]) -> Eip2718Result<ReceiptWithBloom<Self>> {
        Ok(Self::rlp_decode_inner(buf, OpTxType::Legacy)?)
    }
}

impl<T: Encodable> RlpEncodableReceipt for BaseReceipt<T> {
    fn rlp_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        let mut len = self.eip2718_encoded_length_with_bloom(bloom);
        if !self.tx_type().is_legacy() {
            len += Header {
                list: false,
                payload_length: self.eip2718_encoded_length_with_bloom(bloom),
            }
            .length();
        }

        len
    }

    fn rlp_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        if !self.tx_type().is_legacy() {
            Header { list: false, payload_length: self.eip2718_encoded_length_with_bloom(bloom) }
                .encode(out);
        }
        self.eip2718_encode_with_bloom(bloom, out);
    }
}

impl<T: Decodable> RlpDecodableReceipt for BaseReceipt<T> {
    fn rlp_decode_with_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<ReceiptWithBloom<Self>> {
        let header_buf = &mut &**buf;
        let header = Header::decode(header_buf)?;

        // Legacy receipt, reuse initial buffer without advancing
        if header.list {
            return Self::rlp_decode_inner(buf, OpTxType::Legacy);
        }

        // Otherwise, advance the buffer and try decoding type flag followed by receipt
        *buf = *header_buf;

        let remaining = buf.len();
        let tx_type = OpTxType::decode(buf)?;
        let this = Self::rlp_decode_inner(buf, tx_type)?;

        if buf.len() + header.payload_length != remaining {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        Ok(this)
    }
}

impl<T: Encodable + Send + Sync> Encodable for BaseReceipt<T> {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_header_without_bloom().encode(out);
        self.rlp_encode_fields_without_bloom(out);
    }

    fn length(&self) -> usize {
        self.rlp_header_without_bloom().length_with_payload()
    }
}

impl<T: Decodable> Decodable for BaseReceipt<T> {
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

impl<T: Send + Sync + Clone + Debug + Eq + AsRef<Log>> TxReceipt for BaseReceipt<T> {
    type Log = T;

    fn status_or_post_state(&self) -> Eip658Value {
        self.as_receipt().status_or_post_state()
    }

    fn status(&self) -> bool {
        self.as_receipt().status()
    }

    fn bloom(&self) -> Bloom {
        self.as_receipt().bloom()
    }

    fn cumulative_gas_used(&self) -> u64 {
        self.as_receipt().cumulative_gas_used()
    }

    fn logs(&self) -> &[Self::Log] {
        self.as_receipt().logs()
    }

    fn into_logs(self) -> Vec<Self::Log> {
        match self {
            Self::Legacy(receipt)
            | Self::Eip2930(receipt)
            | Self::Eip1559(receipt)
            | Self::Eip7702(receipt) => receipt.logs,
            Self::Deposit(receipt) => receipt.inner.logs,
        }
    }
}

impl<T> Typed2718 for BaseReceipt<T> {
    fn ty(&self) -> u8 {
        self.tx_type().into()
    }
}

impl<T> IsTyped2718 for BaseReceipt<T> {
    fn is_type(type_id: u8) -> bool {
        <OpTxType as IsTyped2718>::is_type(type_id)
    }
}

impl<T: Send + Sync + Clone + Debug + Eq + AsRef<Log>> BaseTxReceipt for BaseReceipt<T> {
    fn deposit_nonce(&self) -> Option<u64> {
        match self {
            Self::Deposit(receipt) => receipt.deposit_nonce,
            _ => None,
        }
    }

    fn deposit_receipt_version(&self) -> Option<u64> {
        match self {
            Self::Deposit(receipt) => receipt.deposit_receipt_version,
            _ => None,
        }
    }
}

impl From<super::BaseReceiptEnvelope> for BaseReceipt {
    fn from(envelope: super::BaseReceiptEnvelope) -> Self {
        match envelope {
            super::BaseReceiptEnvelope::Legacy(receipt) => Self::Legacy(receipt.receipt),
            super::BaseReceiptEnvelope::Eip2930(receipt) => Self::Eip2930(receipt.receipt),
            super::BaseReceiptEnvelope::Eip1559(receipt) => Self::Eip1559(receipt.receipt),
            super::BaseReceiptEnvelope::Eip7702(receipt) => Self::Eip7702(receipt.receipt),
            super::BaseReceiptEnvelope::Deposit(receipt) => Self::Deposit(DepositReceipt {
                deposit_nonce: receipt.receipt.deposit_nonce,
                deposit_receipt_version: receipt.receipt.deposit_receipt_version,
                inner: receipt.receipt.inner,
            }),
        }
    }
}

impl From<ReceiptWithBloom<BaseReceipt>> for BaseReceiptEnvelope {
    fn from(value: ReceiptWithBloom<BaseReceipt>) -> Self {
        let (receipt, logs_bloom) = value.into_components();
        match receipt {
            BaseReceipt::Legacy(receipt) => Self::Legacy(ReceiptWithBloom { receipt, logs_bloom }),
            BaseReceipt::Eip2930(receipt) => {
                Self::Eip2930(ReceiptWithBloom { receipt, logs_bloom })
            }
            BaseReceipt::Eip1559(receipt) => {
                Self::Eip1559(ReceiptWithBloom { receipt, logs_bloom })
            }
            BaseReceipt::Eip7702(receipt) => {
                Self::Eip7702(ReceiptWithBloom { receipt, logs_bloom })
            }
            BaseReceipt::Deposit(receipt) => {
                Self::Deposit(ReceiptWithBloom { receipt, logs_bloom })
            }
        }
    }
}

/// Bincode-compatible serde implementations for opreceipt type.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub(super) mod serde_bincode_compat {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_with::{DeserializeAs, SerializeAs};

    /// Bincode-compatible [`super::BaseReceipt`] serde implementation.
    ///
    /// Intended to use with the [`serde_with::serde_as`] macro in the following way:
    /// ```rust
    /// use base_common_consensus::{BaseReceipt, serde_bincode_compat};
    /// use serde::{Deserialize, Serialize, de::DeserializeOwned};
    /// use serde_with::serde_as;
    ///
    /// #[serde_as]
    /// #[derive(Serialize, Deserialize)]
    /// struct Data {
    ///     #[serde_as(as = "serde_bincode_compat::BaseReceipt<'_>")]
    ///     receipt: BaseReceipt,
    /// }
    /// ```
    #[derive(Debug, Serialize, Deserialize)]
    pub enum BaseReceipt<'a> {
        /// Legacy receipt
        Legacy(alloy_consensus::serde_bincode_compat::Receipt<'a, alloy_primitives::Log>),
        /// EIP-2930 receipt
        Eip2930(alloy_consensus::serde_bincode_compat::Receipt<'a, alloy_primitives::Log>),
        /// EIP-1559 receipt
        Eip1559(alloy_consensus::serde_bincode_compat::Receipt<'a, alloy_primitives::Log>),
        /// EIP-7702 receipt
        Eip7702(alloy_consensus::serde_bincode_compat::Receipt<'a, alloy_primitives::Log>),
        /// Deposit receipt
        Deposit(crate::serde_bincode_compat::DepositReceipt<'a, alloy_primitives::Log>),
    }

    impl<'a> From<&'a super::BaseReceipt> for BaseReceipt<'a> {
        fn from(value: &'a super::BaseReceipt) -> Self {
            match value {
                super::BaseReceipt::Legacy(receipt) => Self::Legacy(receipt.into()),
                super::BaseReceipt::Eip2930(receipt) => Self::Eip2930(receipt.into()),
                super::BaseReceipt::Eip1559(receipt) => Self::Eip1559(receipt.into()),
                super::BaseReceipt::Eip7702(receipt) => Self::Eip7702(receipt.into()),
                super::BaseReceipt::Deposit(receipt) => Self::Deposit(receipt.into()),
            }
        }
    }

    impl<'a> From<BaseReceipt<'a>> for super::BaseReceipt {
        fn from(value: BaseReceipt<'a>) -> Self {
            match value {
                BaseReceipt::Legacy(receipt) => Self::Legacy(receipt.into()),
                BaseReceipt::Eip2930(receipt) => Self::Eip2930(receipt.into()),
                BaseReceipt::Eip1559(receipt) => Self::Eip1559(receipt.into()),
                BaseReceipt::Eip7702(receipt) => Self::Eip7702(receipt.into()),
                BaseReceipt::Deposit(receipt) => Self::Deposit(receipt.into()),
            }
        }
    }

    impl SerializeAs<super::BaseReceipt> for BaseReceipt<'_> {
        fn serialize_as<S>(source: &super::BaseReceipt, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            BaseReceipt::<'_>::from(source).serialize(serializer)
        }
    }

    impl<'de> DeserializeAs<'de, super::BaseReceipt> for BaseReceipt<'de> {
        fn deserialize_as<D>(deserializer: D) -> Result<super::BaseReceipt, D::Error>
        where
            D: Deserializer<'de>,
        {
            BaseReceipt::<'_>::deserialize(deserializer).map(Into::into)
        }
    }

    #[cfg(test)]
    mod tests {
        use arbitrary::Arbitrary;
        use rand::Rng;
        use serde::{Deserialize, Serialize};
        use serde_with::serde_as;

        use crate::BaseReceipt;

        #[test]
        fn test_tx_bincode_roundtrip() {
            #[serde_as]
            #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
            struct Data {
                #[serde_as(as = "super::BaseReceipt<'_>")]
                receipt: BaseReceipt,
            }

            let mut bytes = [0u8; 1024];
            rand::rng().fill(bytes.as_mut_slice());
            let mut data = Data {
                receipt: BaseReceipt::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap(),
            };
            let success = data.receipt.as_receipt_mut().status.coerce_status();
            // // ensure we don't have an invalid poststate variant
            data.receipt.as_receipt_mut().status = success.into();

            let encoded = bincode::serde::encode_to_vec(&data, bincode::config::legacy()).unwrap();
            let (decoded, _) =
                bincode::serde::decode_from_slice::<Data, _>(&encoded, bincode::config::legacy())
                    .unwrap();
            assert_eq!(decoded, data);
        }
    }
}

impl<T> InMemorySize for BaseReceipt<T>
where
    Receipt<T>: InMemorySize,
    DepositReceipt<T>: InMemorySize,
{
    fn size(&self) -> usize {
        match self {
            Self::Legacy(receipt)
            | Self::Eip2930(receipt)
            | Self::Eip1559(receipt)
            | Self::Eip7702(receipt) => receipt.size(),
            Self::Deposit(receipt) => receipt.size(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_eips::Encodable2718;
    use alloy_primitives::{Bytes, address, b256, bytes, hex_literal::hex};
    use alloy_rlp::Encodable;

    use super::*;

    // Test vector from: https://eips.ethereum.org/EIPS/eip-2481
    #[test]
    fn encode_legacy_receipt() {
        let expected = hex!(
            "f901668001b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f85ff85d940000000000000000000000000000000000000011f842a0000000000000000000000000000000000000000000000000000000000000deada0000000000000000000000000000000000000000000000000000000000000beef830100ff"
        );

        let mut data = Vec::with_capacity(expected.length());
        let receipt = ReceiptWithBloom {
            receipt: BaseReceipt::Legacy(Receipt {
                status: Eip658Value::Eip658(false),
                cumulative_gas_used: 0x1,
                logs: vec![Log::new_unchecked(
                    address!("0x0000000000000000000000000000000000000011"),
                    vec![
                        b256!("0x000000000000000000000000000000000000000000000000000000000000dead"),
                        b256!("0x000000000000000000000000000000000000000000000000000000000000beef"),
                    ],
                    bytes!("0100ff"),
                )],
            }),
            logs_bloom: [0; 256].into(),
        };

        receipt.encode(&mut data);

        // check that the rlp length equals the length of the expected rlp
        assert_eq!(receipt.length(), expected.len());
        assert_eq!(data, expected);
    }

    // Test vector from: https://eips.ethereum.org/EIPS/eip-2481
    #[test]
    fn decode_legacy_receipt() {
        let data = hex!(
            "f901668001b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f85ff85d940000000000000000000000000000000000000011f842a0000000000000000000000000000000000000000000000000000000000000deada0000000000000000000000000000000000000000000000000000000000000beef830100ff"
        );

        // EIP658Receipt
        let expected = ReceiptWithBloom {
            receipt: BaseReceipt::Legacy(Receipt {
                status: Eip658Value::Eip658(false),
                cumulative_gas_used: 0x1,
                logs: vec![Log::new_unchecked(
                    address!("0x0000000000000000000000000000000000000011"),
                    vec![
                        b256!("0x000000000000000000000000000000000000000000000000000000000000dead"),
                        b256!("0x000000000000000000000000000000000000000000000000000000000000beef"),
                    ],
                    bytes!("0100ff"),
                )],
            }),
            logs_bloom: [0; 256].into(),
        };

        let receipt = ReceiptWithBloom::decode(&mut &data[..]).unwrap();
        assert_eq!(receipt, expected);
    }

    #[test]
    fn decode_deposit_receipt_regolith_roundtrip() {
        let data = hex!(
            "b901107ef9010c0182b741b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0833d3bbf"
        );

        // Deposit Receipt (post-regolith)
        let expected: ReceiptWithBloom<BaseReceipt> = ReceiptWithBloom {
            receipt: BaseReceipt::Deposit(DepositReceipt {
                inner: Receipt {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: 46913,
                    logs: vec![],
                },
                deposit_nonce: Some(4012991),
                deposit_receipt_version: None,
            }),
            logs_bloom: [0; 256].into(),
        };

        let receipt = ReceiptWithBloom::decode(&mut &data[..]).unwrap();
        assert_eq!(receipt, expected);

        let mut buf = Vec::with_capacity(data.len());
        receipt.encode(&mut buf);
        assert_eq!(buf, &data[..]);
    }

    #[test]
    fn decode_deposit_receipt_canyon_roundtrip() {
        let data = hex!(
            "b901117ef9010d0182b741b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0833d3bbf01"
        );

        // Deposit Receipt (post-canyon)
        let expected: ReceiptWithBloom<BaseReceipt> = ReceiptWithBloom {
            receipt: BaseReceipt::Deposit(DepositReceipt {
                inner: Receipt {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: 46913,
                    logs: vec![],
                },
                deposit_nonce: Some(4012991),
                deposit_receipt_version: Some(1),
            }),
            logs_bloom: [0; 256].into(),
        };

        let receipt = ReceiptWithBloom::decode(&mut &data[..]).unwrap();
        assert_eq!(receipt, expected);

        let mut buf = Vec::with_capacity(data.len());
        expected.encode(&mut buf);
        assert_eq!(buf, &data[..]);
    }

    #[test]
    fn gigantic_receipt() {
        let receipt = BaseReceipt::Legacy(Receipt {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 16747627,
            logs: vec![
                Log::new_unchecked(
                    address!("0x4bf56695415f725e43c3e04354b604bcfb6dfb6e"),
                    vec![b256!(
                        "0xc69dc3d7ebff79e41f525be431d5cd3cc08f80eaf0f7819054a726eeb7086eb9"
                    )],
                    Bytes::from(vec![1; 0xffffff]),
                ),
                Log::new_unchecked(
                    address!("0xfaca325c86bf9c2d5b413cd7b90b209be92229c2"),
                    vec![b256!(
                        "0x8cca58667b1e9ffa004720ac99a3d61a138181963b294d270d91c53d36402ae2"
                    )],
                    Bytes::from(vec![1; 0xffffff]),
                ),
            ],
        });

        let _bloom = receipt.bloom();
        let mut encoded = vec![];
        receipt.encode(&mut encoded);

        let decoded = BaseReceipt::decode(&mut &encoded[..]).unwrap();
        assert_eq!(decoded, receipt);
    }

    #[test]
    fn test_encode_2718_length() {
        let receipt: ReceiptWithBloom<BaseReceipt> = ReceiptWithBloom {
            receipt: BaseReceipt::Eip1559(Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21000,
                logs: vec![],
            }),
            logs_bloom: Bloom::default(),
        };

        let encoded = receipt.encoded_2718();
        assert_eq!(
            encoded.len(),
            receipt.encode_2718_len(),
            "Encoded length should match the actual encoded data length"
        );

        // Test for legacy receipt as well
        let legacy_receipt: ReceiptWithBloom<BaseReceipt> = ReceiptWithBloom {
            receipt: BaseReceipt::Legacy(Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21000,
                logs: vec![],
            }),
            logs_bloom: Bloom::default(),
        };

        let legacy_encoded = legacy_receipt.encoded_2718();
        assert_eq!(
            legacy_encoded.len(),
            legacy_receipt.encode_2718_len(),
            "Encoded length for legacy receipt should match the actual encoded data length"
        );
    }
}
