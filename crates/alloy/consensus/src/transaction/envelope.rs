use alloy_consensus::{
    Signed, TxEip1559, TxEip2930, TxEip4844, TxEip4844Variant, TxEip4844WithSidecar, TxLegacy,
};
use alloy_eips::eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718};
use alloy_rlp::{Decodable, Encodable, Header};
use derive_more::Display;

use crate::TxDeposit;

/// Identifier for an Optimism deposit transaction
pub const DEPOSIT_TX_TYPE_ID: u8 = 126; // 0x7E

/// Optimism `TransactionType` flags as specified in EIPs [2718], [1559], and
/// [2930], as well as the [deposit transaction spec][deposit-spec]
///
/// [2718]: https://eips.ethereum.org/EIPS/eip-2718
/// [1559]: https://eips.ethereum.org/EIPS/eip-1559
/// [2930]: https://eips.ethereum.org/EIPS/eip-2930
/// [4844]: https://eips.ethereum.org/EIPS/eip-4844
/// [deposit-spec]: https://specs.optimism.io/protocol/deposits.html
#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash, Display)]
pub enum OpTxType {
    /// Legacy transaction type.
    #[display(fmt = "legacy")]
    Legacy = 0,
    /// EIP-2930 transaction type.
    #[display(fmt = "eip2930")]
    Eip2930 = 1,
    /// EIP-1559 transaction type.
    #[display(fmt = "eip1559")]
    Eip1559 = 2,
    /// EIP-4844 transaction type.
    #[display(fmt = "eip4844")]
    Eip4844 = 3,
    /// Optimism Deposit transaction type.
    #[display(fmt = "deposit")]
    Deposit = 126,
}

#[cfg(any(test, feature = "arbitrary"))]
impl<'a> arbitrary::Arbitrary<'a> for OpTxType {
    fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=3)? {
            0 => OpTxType::Legacy,
            1 => OpTxType::Eip2930,
            2 => OpTxType::Eip1559,
            3 => OpTxType::Eip4844,
            126 => OpTxType::Deposit,
            _ => unreachable!(),
        })
    }
}

impl TryFrom<u8> for OpTxType {
    type Error = Eip2718Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Legacy,
            1 => Self::Eip2930,
            2 => Self::Eip1559,
            3 => Self::Eip4844,
            126 => Self::Deposit,
            _ => return Err(Eip2718Error::UnexpectedType(value)),
        })
    }
}

/// The Ethereum [EIP-2718] Transaction Envelope, modified for OP Stack chains.
///
/// # Note:
///
/// This enum distinguishes between tagged and untagged legacy transactions, as
/// the in-protocol merkle tree may commit to EITHER 0-prefixed or raw.
/// Therefore we must ensure that encoding returns the precise byte-array that
/// was decoded, preserving the presence or absence of the `TransactionType`
/// flag.
///
/// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type"))]
#[non_exhaustive]
pub enum OpTxEnvelope {
    /// An untagged [`TxLegacy`].
    #[cfg_attr(feature = "serde", serde(rename = "0x0", alias = "0x00"))]
    Legacy(Signed<TxLegacy>),
    /// A [`TxEip2930`] tagged with type 1.
    #[cfg_attr(feature = "serde", serde(rename = "0x1", alias = "0x01"))]
    Eip2930(Signed<TxEip2930>),
    /// A [`TxEip1559`] tagged with type 2.
    #[cfg_attr(feature = "serde", serde(rename = "0x2", alias = "0x02"))]
    Eip1559(Signed<TxEip1559>),
    /// A TxEip4844 tagged with type 3.
    /// An EIP-4844 transaction has two network representations:
    /// 1 - The transaction itself, which is a regular RLP-encoded transaction and used to retrieve
    /// historical transactions..
    ///
    /// 2 - The transaction with a sidecar, which is the form used to
    /// send transactions to the network.
    #[cfg_attr(feature = "serde", serde(rename = "0x3", alias = "0x03"))]
    Eip4844(Signed<TxEip4844Variant>),
    /// A [`TxDeposit`] tagged with type 0x7E.
    #[cfg_attr(feature = "serde", serde(rename = "0x7E", alias = "0x7E"))]
    Deposit(TxDeposit),
}

impl From<Signed<TxLegacy>> for OpTxEnvelope {
    fn from(v: Signed<TxLegacy>) -> Self {
        Self::Legacy(v)
    }
}

impl From<Signed<TxEip2930>> for OpTxEnvelope {
    fn from(v: Signed<TxEip2930>) -> Self {
        Self::Eip2930(v)
    }
}

impl From<Signed<TxEip1559>> for OpTxEnvelope {
    fn from(v: Signed<TxEip1559>) -> Self {
        Self::Eip1559(v)
    }
}

impl From<Signed<TxEip4844Variant>> for OpTxEnvelope {
    fn from(v: Signed<TxEip4844Variant>) -> Self {
        Self::Eip4844(v)
    }
}

impl From<Signed<TxEip4844>> for OpTxEnvelope {
    fn from(v: Signed<TxEip4844>) -> Self {
        let (tx, signature, hash) = v.into_parts();
        Self::Eip4844(Signed::new_unchecked(TxEip4844Variant::TxEip4844(tx), signature, hash))
    }
}

impl From<Signed<TxEip4844WithSidecar>> for OpTxEnvelope {
    fn from(v: Signed<TxEip4844WithSidecar>) -> Self {
        let (tx, signature, hash) = v.into_parts();
        Self::Eip4844(Signed::new_unchecked(
            TxEip4844Variant::TxEip4844WithSidecar(tx),
            signature,
            hash,
        ))
    }
}

impl From<TxDeposit> for OpTxEnvelope {
    fn from(v: TxDeposit) -> Self {
        Self::Deposit(v)
    }
}

impl OpTxEnvelope {
    /// Return the [`OpTxType`] of the inner txn.
    pub const fn tx_type(&self) -> OpTxType {
        match self {
            Self::Legacy(_) => OpTxType::Legacy,
            Self::Eip2930(_) => OpTxType::Eip2930,
            Self::Eip1559(_) => OpTxType::Eip1559,
            Self::Eip4844(_) => OpTxType::Eip4844,
            Self::Deposit(_) => OpTxType::Deposit,
        }
    }

    /// Return the length of the inner txn, __without a type byte__.
    pub fn inner_length(&self) -> usize {
        match self {
            Self::Legacy(t) => t.tx().fields_len() + t.signature().rlp_vrs_len(),
            Self::Eip2930(t) => {
                let payload_length = t.tx().fields_len() + t.signature().rlp_vrs_len();
                Header { list: true, payload_length }.length() + payload_length
            }
            Self::Eip1559(t) => {
                let payload_length = t.tx().fields_len() + t.signature().rlp_vrs_len();
                Header { list: true, payload_length }.length() + payload_length
            }
            Self::Eip4844(t) => match t.tx() {
                TxEip4844Variant::TxEip4844(tx) => {
                    let payload_length = tx.fields_len() + t.signature().rlp_vrs_len();
                    Header { list: true, payload_length }.length() + payload_length
                }
                TxEip4844Variant::TxEip4844WithSidecar(tx) => {
                    let inner_payload_length = tx.tx().fields_len() + t.signature().rlp_vrs_len();
                    let inner_header = Header { list: true, payload_length: inner_payload_length };

                    let outer_payload_length =
                        inner_header.length() + inner_payload_length + tx.sidecar.fields_len();
                    let outer_header = Header { list: true, payload_length: outer_payload_length };

                    outer_header.length() + outer_payload_length
                }
            },
            Self::Deposit(t) => {
                let payload_length = t.fields_len();
                Header { list: true, payload_length }.length() + payload_length
            }
        }
    }

    /// Return the RLP payload length of the network-serialized wrapper
    fn rlp_payload_length(&self) -> usize {
        if let Self::Legacy(t) = self {
            let payload_length = t.tx().fields_len() + t.signature().rlp_vrs_len();
            return Header { list: true, payload_length }.length() + payload_length;
        }
        // length of inner tx body
        let inner_length = self.inner_length();
        // with tx type byte
        inner_length + 1
    }
}

impl Encodable for OpTxEnvelope {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.network_encode(out)
    }

    fn length(&self) -> usize {
        let mut payload_length = self.rlp_payload_length();
        if !self.is_legacy() {
            payload_length += Header { list: false, payload_length }.length();
        }

        payload_length
    }
}

impl Decodable for OpTxEnvelope {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        match Self::network_decode(buf) {
            Ok(t) => Ok(t),
            Err(Eip2718Error::RlpError(e)) => Err(e),
            Err(Eip2718Error::UnexpectedType(_)) => {
                Err(alloy_rlp::Error::Custom("unexpected tx type"))
            }
            _ => Err(alloy_rlp::Error::Custom("unknown error decoding tx envelope")),
        }
    }
}

impl Decodable2718 for OpTxEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        match ty.try_into().map_err(|_| Eip2718Error::UnexpectedType(ty))? {
            OpTxType::Eip2930 => Ok(Self::Eip2930(TxEip2930::decode_signed_fields(buf)?)),
            OpTxType::Eip1559 => Ok(Self::Eip1559(TxEip1559::decode_signed_fields(buf)?)),
            OpTxType::Eip4844 => Ok(Self::Eip4844(TxEip4844Variant::decode_signed_fields(buf)?)),
            OpTxType::Deposit => Ok(Self::Deposit(TxDeposit::decode(buf)?)),
            OpTxType::Legacy => {
                Err(alloy_rlp::Error::Custom("type-0 eip2718 transactions are not supported")
                    .into())
            }
        }
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(OpTxEnvelope::Legacy(TxLegacy::decode_signed_fields(buf)?))
    }
}

impl Encodable2718 for OpTxEnvelope {
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

    fn encode_2718(&self, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            // Legacy transactions have no difference between network and 2718
            OpTxEnvelope::Legacy(tx) => tx.tx().encode_with_signature_fields(tx.signature(), out),
            OpTxEnvelope::Eip2930(tx) => {
                tx.tx().encode_with_signature(tx.signature(), out, false);
            }
            OpTxEnvelope::Eip1559(tx) => {
                tx.tx().encode_with_signature(tx.signature(), out, false);
            }
            OpTxEnvelope::Eip4844(tx) => {
                tx.tx().encode_with_signature(tx.signature(), out, false);
            }
            OpTxEnvelope::Deposit(tx) => {
                out.put_u8(OpTxType::Deposit as u8);
                tx.encode(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes, TxKind, B256, U256};

    #[cfg(not(feature = "std"))]
    use alloc::vec;

    #[test]
    fn test_encode_decode_deposit() {
        let tx = TxDeposit {
            source_hash: B256::left_padding_from(&[0xde, 0xad]),
            from: Address::left_padding_from(&[0xbe, 0xef]),
            mint: Some(1),
            gas_limit: 2,
            to: TxKind::Call(Address::left_padding_from(&[3])),
            value: U256::from(4_u64),
            input: Bytes::from(vec![5]),
            is_system_transaction: false,
        };
        let tx_envelope = OpTxEnvelope::Deposit(tx);
        let encoded = tx_envelope.encoded_2718();
        let decoded = OpTxEnvelope::decode_2718(&mut encoded.as_ref()).unwrap();
        assert_eq!(encoded.len(), tx_envelope.encode_2718_len());
        assert_eq!(decoded, tx_envelope);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_serde_roundtrip_deposit() {
        let tx = TxDeposit {
            gas_limit: u128::MAX,
            to: TxKind::Call(Address::random()),
            value: U256::MAX,
            input: Bytes::new(),
            source_hash: U256::MAX.into(),
            from: Address::random(),
            mint: Some(u128::MAX),
            is_system_transaction: false,
        };
        let tx_envelope = OpTxEnvelope::Deposit(tx);

        let serialized = serde_json::to_string(&tx_envelope).unwrap();
        let deserialized: OpTxEnvelope = serde_json::from_str(&serialized).unwrap();

        assert_eq!(tx_envelope, deserialized);
    }
}
