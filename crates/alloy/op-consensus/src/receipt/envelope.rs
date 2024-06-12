use crate::{OpDepositReceipt, OpReceiptWithBloom, OpTxType};
use alloy_eips::eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718};
use alloy_primitives::{Bloom, Log};
use alloy_rlp::{length_of_length, BufMut, Decodable, Encodable};

/// Receipt envelope, as defined in [EIP-2718], modified for OP Stack chains.
///
/// This enum distinguishes between tagged and untagged legacy receipts, as the
/// in-protocol merkle tree may commit to EITHER 0-prefixed or raw. Therefore
/// we must ensure that encoding returns the precise byte-array that was
/// decoded, preserving the presence or absence of the `TransactionType` flag.
///
/// Transaction receipt payloads are specified in their respective EIPs.
///
/// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type"))]
#[non_exhaustive]
pub enum OpReceiptEnvelope<T = Log> {
    /// Receipt envelope with no type flag.
    #[cfg_attr(feature = "serde", serde(rename = "0x0", alias = "0x00"))]
    Legacy(OpReceiptWithBloom<T>),
    /// Receipt envelope with type flag 1, containing a [EIP-2930] receipt.
    ///
    /// [EIP-2930]: https://eips.ethereum.org/EIPS/eip-2930
    #[cfg_attr(feature = "serde", serde(rename = "0x1", alias = "0x01"))]
    Eip2930(OpReceiptWithBloom<T>),
    /// Receipt envelope with type flag 2, containing a [EIP-1559] receipt.
    ///
    /// [EIP-1559]: https://eips.ethereum.org/EIPS/eip-1559
    #[cfg_attr(feature = "serde", serde(rename = "0x2", alias = "0x02"))]
    Eip1559(OpReceiptWithBloom<T>),
    /// Receipt envelope with type flag 3, containing a [EIP-4844] receipt.
    ///
    /// [EIP-4844]: https://eips.ethereum.org/EIPS/eip-4844
    #[cfg_attr(feature = "serde", serde(rename = "0x3", alias = "0x03"))]
    Eip4844(OpReceiptWithBloom<T>),
    /// Receipt envelope with type flag 126, containing a [deposit] receipt.
    ///
    /// [deposit]: https://specs.optimism.io/protocol/deposits.html
    #[cfg_attr(feature = "serde", serde(rename = "0x7E", alias = "0x7E"))]
    Deposit(OpReceiptWithBloom<T>),
}

impl<T> OpReceiptEnvelope<T> {
    /// Return the [`OpTxType`] of the inner receipt.
    pub const fn tx_type(&self) -> OpTxType {
        match self {
            Self::Legacy(_) => OpTxType::Legacy,
            Self::Eip2930(_) => OpTxType::Eip2930,
            Self::Eip1559(_) => OpTxType::Eip1559,
            Self::Eip4844(_) => OpTxType::Eip4844,
            Self::Deposit(_) => OpTxType::Deposit,
        }
    }

    /// Return true if the transaction was successful.
    pub fn is_success(&self) -> bool {
        self.status()
    }

    /// Returns the success status of the receipt's transaction.
    pub fn status(&self) -> bool {
       todo!()
    }

    /// Returns the cumulative gas used at this receipt.
    pub fn cumulative_gas_used(&self) -> u128 {
        self.as_receipt().unwrap().inner.cumulative_gas_used
    }

    /// Return the receipt logs.
    pub fn logs(&self) -> &[T] {
        &self.as_receipt().unwrap().inner.logs
    }

    /// Return the receipt's bloom.
    pub fn logs_bloom(&self) -> &Bloom {
        &self.as_receipt_with_bloom().unwrap().logs_bloom
    }

    /// Return the receipt's deposit_nonce.
    pub fn deposit_nonce(&self) -> Option<u64> {
        self.as_receipt().unwrap().deposit_nonce
    }

    /// Return the receipt's deposit version.
    pub fn deposit_receipt_version(&self) -> Option<u64> {
        self.as_receipt().unwrap().deposit_receipt_version
    }

    /// Return the inner receipt with bloom. Currently this is infallible,
    /// however, future receipt types may be added.
    pub const fn as_receipt_with_bloom(&self) -> Option<&OpReceiptWithBloom<T>> {
        match self {
            Self::Legacy(t)
            | Self::Eip2930(t)
            | Self::Eip1559(t)
            | Self::Eip4844(t)
            | Self::Deposit(t) => Some(t),
        }
    }

    /// Return the inner receipt. Currently this is infallible, however, future
    /// receipt types may be added.
    pub const fn as_receipt(&self) -> Option<&OpDepositReceipt<T>> {
        match self {
            Self::Legacy(t)
            | Self::Eip2930(t)
            | Self::Eip1559(t)
            | Self::Eip4844(t)
            | Self::Deposit(t) => Some(&t.receipt),
        }
    }
}

impl OpReceiptEnvelope {
    /// Get the length of the inner receipt in the 2718 encoding.
    pub fn inner_length(&self) -> usize {
        self.as_receipt_with_bloom().unwrap().length()
    }

    /// Calculate the length of the rlp payload of the network encoded receipt.
    pub fn rlp_payload_length(&self) -> usize {
        let length = self.as_receipt_with_bloom().unwrap().length();
        match self {
            Self::Legacy(_) => length,
            _ => length + 1,
        }
    }
}

impl Encodable for OpReceiptEnvelope {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.network_encode(out)
    }

    fn length(&self) -> usize {
        let mut payload_length = self.rlp_payload_length();
        if !self.is_legacy() {
            payload_length += length_of_length(payload_length);
        }
        payload_length
    }
}

impl Decodable for OpReceiptEnvelope {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        match Self::network_decode(buf) {
            Ok(t) => Ok(t),
            Err(_) => Err(alloy_rlp::Error::Custom("Unexpected type")),
        }
    }
}

impl Encodable2718 for OpReceiptEnvelope {
    fn type_flag(&self) -> Option<u8> {
        match self {
            Self::Legacy(_) => None,
            Self::Eip2930(_) => Some(OpTxType::Eip2930 as u8),
            Self::Eip1559(_) => Some(OpTxType::Eip1559 as u8),
            Self::Eip4844(_) => Some(OpTxType::Eip4844 as u8),
            Self::Deposit(_) => Some(OpTxType::Deposit as u8),
        }
    }

    fn encode_2718_len(&self) -> usize {
        self.inner_length() + !self.is_legacy() as usize
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        match self.type_flag() {
            None => {}
            Some(ty) => out.put_u8(ty),
        }
        self.as_receipt_with_bloom().unwrap().encode(out);
    }
}

impl Decodable2718 for OpReceiptEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        let receipt = Decodable::decode(buf)?;
        match ty.try_into().map_err(|_| Eip2718Error::UnexpectedType(ty))? {
            OpTxType::Legacy => {
                Err(alloy_rlp::Error::Custom("type-0 eip2718 transactions are not supported")
                    .into())
            }
            OpTxType::Eip2930 => Ok(Self::Eip2930(receipt)),
            OpTxType::Eip1559 => Ok(Self::Eip1559(receipt)),
            OpTxType::Eip4844 => Ok(Self::Eip4844(receipt)),
            OpTxType::Deposit => Ok(Self::Deposit(receipt)),
        }
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self::Legacy(Decodable::decode(buf)?))
    }
}

#[cfg(all(test, feature = "arbitrary"))]
impl<'a, T> arbitrary::Arbitrary<'a> for OpReceiptEnvelope<T>
where
    T: arbitrary::Arbitrary<'a>,
{
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let receipt = OpReceiptWithBloom::<T>::arbitrary(u)?;

        match u.int_in_range(0..=4)? {
            0 => Ok(Self::Legacy(receipt)),
            1 => Ok(Self::Eip2930(receipt)),
            2 => Ok(Self::Eip1559(receipt)),
            3 => Ok(Self::Eip4844(receipt)),
            _ => Ok(Self::Deposit(receipt)),
        }
    }
}
