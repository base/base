use alloy_consensus::{
    transaction::RlpEcdsaTx, Sealable, Sealed, Signed, Transaction, TxEip1559, TxEip2930,
    TxEip7702, TxLegacy,
};
use alloy_eips::{
    eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718},
    eip2930::AccessList,
    eip7702::SignedAuthorization,
};
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use alloy_rlp::{BufMut, Decodable, Encodable};
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
    #[display("legacy")]
    Legacy = 0,
    /// EIP-2930 transaction type.
    #[display("eip2930")]
    Eip2930 = 1,
    /// EIP-1559 transaction type.
    #[display("eip1559")]
    Eip1559 = 2,
    /// EIP-7702 transaction type.
    #[display("eip7702")]
    Eip7702 = 4,
    /// Optimism Deposit transaction type.
    #[display("deposit")]
    Deposit = 126,
}

impl OpTxType {
    /// List of all variants.
    pub const ALL: [Self; 5] =
        [Self::Legacy, Self::Eip2930, Self::Eip1559, Self::Eip7702, Self::Deposit];
}

#[cfg(any(test, feature = "arbitrary"))]
impl arbitrary::Arbitrary<'_> for OpTxType {
    fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        let i = u.choose_index(Self::ALL.len())?;
        Ok(Self::ALL[i])
    }
}

impl From<OpTxType> for u8 {
    fn from(v: OpTxType) -> Self {
        v as Self
    }
}

impl TryFrom<u8> for OpTxType {
    type Error = Eip2718Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Legacy,
            1 => Self::Eip2930,
            2 => Self::Eip1559,
            4 => Self::Eip7702,
            126 => Self::Deposit,
            _ => return Err(Eip2718Error::UnexpectedType(value)),
        })
    }
}

impl PartialEq<u8> for OpTxType {
    fn eq(&self, other: &u8) -> bool {
        (*self as u8) == *other
    }
}

impl PartialEq<OpTxType> for u8 {
    fn eq(&self, other: &OpTxType) -> bool {
        *self == *other as Self
    }
}

impl Encodable for OpTxType {
    fn encode(&self, out: &mut dyn BufMut) {
        (*self as u8).encode(out);
    }

    fn length(&self) -> usize {
        1
    }
}

impl Decodable for OpTxType {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let ty = u8::decode(buf)?;

        Self::try_from(ty).map_err(|_| alloy_rlp::Error::Custom("invalid transaction type"))
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
#[cfg_attr(
    feature = "serde",
    serde(into = "serde_from::TaggedTxEnvelope", from = "serde_from::MaybeTaggedTxEnvelope")
)]
#[cfg_attr(all(any(test, feature = "arbitrary"), feature = "k256"), derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub enum OpTxEnvelope {
    /// An untagged [`TxLegacy`].
    Legacy(Signed<TxLegacy>),
    /// A [`TxEip2930`] tagged with type 1.
    Eip2930(Signed<TxEip2930>),
    /// A [`TxEip1559`] tagged with type 2.
    Eip1559(Signed<TxEip1559>),
    /// A [`TxEip7702`] tagged with type 4.
    Eip7702(Signed<TxEip7702>),
    /// A [`TxDeposit`] tagged with type 0x7E.
    Deposit(Sealed<TxDeposit>),
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

impl From<Signed<TxEip7702>> for OpTxEnvelope {
    fn from(v: Signed<TxEip7702>) -> Self {
        Self::Eip7702(v)
    }
}

impl From<TxDeposit> for OpTxEnvelope {
    fn from(v: TxDeposit) -> Self {
        v.seal_slow().into()
    }
}

impl From<Sealed<TxDeposit>> for OpTxEnvelope {
    fn from(v: Sealed<TxDeposit>) -> Self {
        Self::Deposit(v)
    }
}

impl Transaction for OpTxEnvelope {
    fn chain_id(&self) -> Option<u64> {
        match self {
            Self::Legacy(tx) => tx.tx().chain_id(),
            Self::Eip2930(tx) => tx.tx().chain_id(),
            Self::Eip1559(tx) => tx.tx().chain_id(),
            Self::Eip7702(tx) => tx.tx().chain_id(),
            Self::Deposit(tx) => tx.chain_id(),
        }
    }

    fn nonce(&self) -> u64 {
        match self {
            Self::Legacy(tx) => tx.tx().nonce(),
            Self::Eip2930(tx) => tx.tx().nonce(),
            Self::Eip1559(tx) => tx.tx().nonce(),
            Self::Eip7702(tx) => tx.tx().nonce(),
            Self::Deposit(tx) => tx.nonce(),
        }
    }

    fn gas_limit(&self) -> u64 {
        match self {
            Self::Legacy(tx) => tx.tx().gas_limit(),
            Self::Eip2930(tx) => tx.tx().gas_limit(),
            Self::Eip1559(tx) => tx.tx().gas_limit(),
            Self::Eip7702(tx) => tx.tx().gas_limit(),
            Self::Deposit(tx) => tx.gas_limit(),
        }
    }

    fn gas_price(&self) -> Option<u128> {
        match self {
            Self::Legacy(tx) => tx.tx().gas_price(),
            Self::Eip2930(tx) => tx.tx().gas_price(),
            Self::Eip1559(tx) => tx.tx().gas_price(),
            Self::Eip7702(tx) => tx.tx().gas_price(),
            Self::Deposit(tx) => tx.gas_price(),
        }
    }

    fn max_fee_per_gas(&self) -> u128 {
        match self {
            Self::Legacy(tx) => tx.tx().max_fee_per_gas(),
            Self::Eip2930(tx) => tx.tx().max_fee_per_gas(),
            Self::Eip1559(tx) => tx.tx().max_fee_per_gas(),
            Self::Eip7702(tx) => tx.tx().max_fee_per_gas(),
            Self::Deposit(tx) => tx.max_fee_per_gas(),
        }
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        match self {
            Self::Legacy(tx) => tx.tx().max_priority_fee_per_gas(),
            Self::Eip2930(tx) => tx.tx().max_priority_fee_per_gas(),
            Self::Eip1559(tx) => tx.tx().max_priority_fee_per_gas(),
            Self::Eip7702(tx) => tx.tx().max_priority_fee_per_gas(),
            Self::Deposit(tx) => tx.max_priority_fee_per_gas(),
        }
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        match self {
            Self::Legacy(tx) => tx.tx().max_fee_per_blob_gas(),
            Self::Eip2930(tx) => tx.tx().max_fee_per_blob_gas(),
            Self::Eip1559(tx) => tx.tx().max_fee_per_blob_gas(),
            Self::Eip7702(tx) => tx.tx().max_fee_per_blob_gas(),
            Self::Deposit(tx) => tx.max_fee_per_blob_gas(),
        }
    }

    fn priority_fee_or_price(&self) -> u128 {
        match self {
            Self::Legacy(tx) => tx.tx().priority_fee_or_price(),
            Self::Eip2930(tx) => tx.tx().priority_fee_or_price(),
            Self::Eip1559(tx) => tx.tx().priority_fee_or_price(),
            Self::Eip7702(tx) => tx.tx().priority_fee_or_price(),
            Self::Deposit(tx) => tx.priority_fee_or_price(),
        }
    }

    fn to(&self) -> Option<Address> {
        match self {
            Self::Legacy(tx) => tx.tx().to(),
            Self::Eip2930(tx) => tx.tx().to(),
            Self::Eip1559(tx) => tx.tx().to(),
            Self::Eip7702(tx) => tx.tx().to(),
            Self::Deposit(tx) => tx.to(),
        }
    }

    fn kind(&self) -> TxKind {
        match self {
            Self::Legacy(tx) => tx.tx().kind(),
            Self::Eip2930(tx) => tx.tx().kind(),
            Self::Eip1559(tx) => tx.tx().kind(),
            Self::Eip7702(tx) => tx.tx().kind(),
            Self::Deposit(tx) => tx.kind(),
        }
    }

    fn value(&self) -> U256 {
        match self {
            Self::Legacy(tx) => tx.tx().value(),
            Self::Eip2930(tx) => tx.tx().value(),
            Self::Eip1559(tx) => tx.tx().value(),
            Self::Eip7702(tx) => tx.tx().value(),
            Self::Deposit(tx) => tx.value(),
        }
    }

    fn input(&self) -> &Bytes {
        match self {
            Self::Legacy(tx) => tx.tx().input(),
            Self::Eip2930(tx) => tx.tx().input(),
            Self::Eip1559(tx) => tx.tx().input(),
            Self::Eip7702(tx) => tx.tx().input(),
            Self::Deposit(tx) => tx.input(),
        }
    }

    fn ty(&self) -> u8 {
        match self {
            Self::Legacy(tx) => tx.tx().ty(),
            Self::Eip2930(tx) => tx.tx().ty(),
            Self::Eip1559(tx) => tx.tx().ty(),
            Self::Eip7702(tx) => tx.tx().ty(),
            Self::Deposit(tx) => tx.ty(),
        }
    }

    fn access_list(&self) -> Option<&AccessList> {
        match self {
            Self::Legacy(tx) => tx.tx().access_list(),
            Self::Eip2930(tx) => tx.tx().access_list(),
            Self::Eip1559(tx) => tx.tx().access_list(),
            Self::Eip7702(tx) => tx.tx().access_list(),
            Self::Deposit(tx) => tx.access_list(),
        }
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        match self {
            Self::Legacy(tx) => tx.tx().blob_versioned_hashes(),
            Self::Eip2930(tx) => tx.tx().blob_versioned_hashes(),
            Self::Eip1559(tx) => tx.tx().blob_versioned_hashes(),
            Self::Eip7702(tx) => tx.tx().blob_versioned_hashes(),
            Self::Deposit(tx) => tx.blob_versioned_hashes(),
        }
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        match self {
            Self::Legacy(tx) => tx.tx().authorization_list(),
            Self::Eip2930(tx) => tx.tx().authorization_list(),
            Self::Eip1559(tx) => tx.tx().authorization_list(),
            Self::Eip7702(tx) => tx.tx().authorization_list(),
            Self::Deposit(tx) => tx.authorization_list(),
        }
    }

    fn is_dynamic_fee(&self) -> bool {
        match self {
            Self::Legacy(tx) => tx.tx().is_dynamic_fee(),
            Self::Eip2930(tx) => tx.tx().is_dynamic_fee(),
            Self::Eip1559(tx) => tx.tx().is_dynamic_fee(),
            Self::Eip7702(tx) => tx.tx().is_dynamic_fee(),
            Self::Deposit(tx) => tx.is_dynamic_fee(),
        }
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        match self {
            Self::Legacy(tx) => tx.tx().effective_gas_price(base_fee),
            Self::Eip2930(tx) => tx.tx().effective_gas_price(base_fee),
            Self::Eip1559(tx) => tx.tx().effective_gas_price(base_fee),
            Self::Eip7702(tx) => tx.tx().effective_gas_price(base_fee),
            Self::Deposit(tx) => tx.effective_gas_price(base_fee),
        }
    }
}

impl OpTxEnvelope {
    /// Returns true if the transaction is a legacy transaction.
    #[inline]
    pub const fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy(_))
    }

    /// Returns true if the transaction is an EIP-2930 transaction.
    #[inline]
    pub const fn is_eip2930(&self) -> bool {
        matches!(self, Self::Eip2930(_))
    }

    /// Returns true if the transaction is an EIP-1559 transaction.
    #[inline]
    pub const fn is_eip1559(&self) -> bool {
        matches!(self, Self::Eip1559(_))
    }

    /// Returns true if the transaction is a deposit transaction.
    #[inline]
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit(_))
    }

    /// Returns true if the transaction is a system transaction.
    #[inline]
    pub const fn is_system_transaction(&self) -> bool {
        match self {
            Self::Deposit(tx) => tx.inner().is_system_transaction,
            _ => false,
        }
    }

    /// Returns the [`TxLegacy`] variant if the transaction is a legacy transaction.
    pub const fn as_legacy(&self) -> Option<&Signed<TxLegacy>> {
        match self {
            Self::Legacy(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip2930`] variant if the transaction is an EIP-2930 transaction.
    pub const fn as_eip2930(&self) -> Option<&Signed<TxEip2930>> {
        match self {
            Self::Eip2930(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip1559`] variant if the transaction is an EIP-1559 transaction.
    pub const fn as_eip1559(&self) -> Option<&Signed<TxEip1559>> {
        match self {
            Self::Eip1559(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxDeposit`] variant if the transaction is a deposit transaction.
    pub const fn as_deposit(&self) -> Option<&Sealed<TxDeposit>> {
        match self {
            Self::Deposit(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the [`OpTxType`] of the inner txn.
    pub const fn tx_type(&self) -> OpTxType {
        match self {
            Self::Legacy(_) => OpTxType::Legacy,
            Self::Eip2930(_) => OpTxType::Eip2930,
            Self::Eip1559(_) => OpTxType::Eip1559,
            Self::Eip7702(_) => OpTxType::Eip7702,
            Self::Deposit(_) => OpTxType::Deposit,
        }
    }

    /// Return the length of the inner txn, including type byte length
    pub fn eip2718_encoded_length(&self) -> usize {
        match self {
            Self::Legacy(t) => t.eip2718_encoded_length(),
            Self::Eip2930(t) => t.eip2718_encoded_length(),
            Self::Eip1559(t) => t.eip2718_encoded_length(),
            Self::Eip7702(t) => t.eip2718_encoded_length(),
            Self::Deposit(t) => t.eip2718_encoded_length(),
        }
    }
}

impl Encodable for OpTxEnvelope {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.network_encode(out)
    }

    fn length(&self) -> usize {
        self.network_len()
    }
}

impl Decodable for OpTxEnvelope {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self::network_decode(buf)?)
    }
}

impl Decodable2718 for OpTxEnvelope {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        match ty.try_into().map_err(|_| Eip2718Error::UnexpectedType(ty))? {
            OpTxType::Eip2930 => Ok(Self::Eip2930(TxEip2930::rlp_decode_signed(buf)?)),
            OpTxType::Eip1559 => Ok(Self::Eip1559(TxEip1559::rlp_decode_signed(buf)?)),
            OpTxType::Eip7702 => Ok(Self::Eip7702(TxEip7702::rlp_decode_signed(buf)?)),
            OpTxType::Deposit => Ok(Self::Deposit(TxDeposit::decode(buf)?.seal_slow())),
            OpTxType::Legacy => {
                Err(alloy_rlp::Error::Custom("type-0 eip2718 transactions are not supported")
                    .into())
            }
        }
    }

    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self::Legacy(TxLegacy::rlp_decode_signed(buf)?))
    }
}

impl Encodable2718 for OpTxEnvelope {
    fn type_flag(&self) -> Option<u8> {
        match self {
            Self::Legacy(_) => None,
            Self::Eip2930(_) => Some(OpTxType::Eip2930 as u8),
            Self::Eip1559(_) => Some(OpTxType::Eip1559 as u8),
            Self::Eip7702(_) => Some(OpTxType::Eip7702 as u8),
            Self::Deposit(_) => Some(OpTxType::Deposit as u8),
        }
    }

    fn encode_2718_len(&self) -> usize {
        self.eip2718_encoded_length()
    }

    fn encode_2718(&self, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            // Legacy transactions have no difference between network and 2718
            Self::Legacy(tx) => tx.eip2718_encode(out),
            Self::Eip2930(tx) => {
                tx.eip2718_encode(out);
            }
            Self::Eip1559(tx) => {
                tx.eip2718_encode(out);
            }
            Self::Eip7702(tx) => {
                tx.eip2718_encode(out);
            }
            Self::Deposit(tx) => {
                tx.eip2718_encode(out);
            }
        }
    }

    fn trie_hash(&self) -> B256 {
        match self {
            Self::Legacy(tx) => *tx.hash(),
            Self::Eip1559(tx) => *tx.hash(),
            Self::Eip2930(tx) => *tx.hash(),
            Self::Eip7702(tx) => *tx.hash(),
            Self::Deposit(tx) => tx.seal(),
        }
    }
}

#[cfg(feature = "serde")]
mod serde_from {
    //! NB: Why do we need this?
    //!
    //! Because the tag may be missing, we need an abstraction over tagged (with
    //! type) and untagged (always legacy). This is [`MaybeTaggedTxEnvelope`].
    //!
    //! The tagged variant is [`TaggedTxEnvelope`], which always has a type tag.
    //!
    //! We serialize via [`TaggedTxEnvelope`] and deserialize via
    //! [`MaybeTaggedTxEnvelope`].
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    #[serde(untagged)]
    pub(crate) enum MaybeTaggedTxEnvelope {
        Tagged(TaggedTxEnvelope),
        #[serde(with = "alloy_consensus::transaction::signed_legacy_serde")]
        Untagged(Signed<TxLegacy>),
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "type")]
    pub(crate) enum TaggedTxEnvelope {
        #[serde(
            rename = "0x0",
            alias = "0x00",
            with = "alloy_consensus::transaction::signed_legacy_serde"
        )]
        Legacy(Signed<TxLegacy>),
        #[serde(rename = "0x1", alias = "0x01")]
        Eip2930(Signed<TxEip2930>),
        #[serde(rename = "0x2", alias = "0x02")]
        Eip1559(Signed<TxEip1559>),
        #[serde(rename = "0x4", alias = "0x04")]
        Eip7702(Signed<TxEip7702>),
        #[serde(rename = "0x7e", alias = "0x7E", serialize_with = "crate::serde_deposit_tx_rpc")]
        Deposit(Sealed<TxDeposit>),
    }

    impl From<MaybeTaggedTxEnvelope> for OpTxEnvelope {
        fn from(value: MaybeTaggedTxEnvelope) -> Self {
            match value {
                MaybeTaggedTxEnvelope::Tagged(tagged) => tagged.into(),
                MaybeTaggedTxEnvelope::Untagged(tx) => Self::Legacy(tx),
            }
        }
    }

    impl From<TaggedTxEnvelope> for OpTxEnvelope {
        fn from(value: TaggedTxEnvelope) -> Self {
            match value {
                TaggedTxEnvelope::Legacy(signed) => Self::Legacy(signed),
                TaggedTxEnvelope::Eip2930(signed) => Self::Eip2930(signed),
                TaggedTxEnvelope::Eip1559(signed) => Self::Eip1559(signed),
                TaggedTxEnvelope::Eip7702(signed) => Self::Eip7702(signed),
                TaggedTxEnvelope::Deposit(tx) => Self::Deposit(tx),
            }
        }
    }

    impl From<OpTxEnvelope> for TaggedTxEnvelope {
        fn from(value: OpTxEnvelope) -> Self {
            match value {
                OpTxEnvelope::Legacy(signed) => Self::Legacy(signed),
                OpTxEnvelope::Eip2930(signed) => Self::Eip2930(signed),
                OpTxEnvelope::Eip1559(signed) => Self::Eip1559(signed),
                OpTxEnvelope::Eip7702(signed) => Self::Eip7702(signed),
                OpTxEnvelope::Deposit(tx) => Self::Deposit(tx),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloy_primitives::{hex, Address, Bytes, TxKind, B256, U256};

    #[test]
    fn test_tx_gas_limit() {
        let tx = TxDeposit { gas_limit: 1, ..Default::default() };
        let tx_envelope = OpTxEnvelope::Deposit(tx.seal_slow());
        assert_eq!(tx_envelope.gas_limit(), 1);
    }

    #[test]
    fn test_system_transaction() {
        let mut tx = TxDeposit { is_system_transaction: true, ..Default::default() };
        let tx_envelope = OpTxEnvelope::Deposit(tx.clone().seal_slow());
        assert!(tx_envelope.is_system_transaction());

        tx.is_system_transaction = false;
        let tx_envelope = OpTxEnvelope::Deposit(tx.seal_slow());
        assert!(!tx_envelope.is_system_transaction());
    }

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
        let tx_envelope = OpTxEnvelope::Deposit(tx.seal_slow());
        let encoded = tx_envelope.encoded_2718();
        let decoded = OpTxEnvelope::decode_2718(&mut encoded.as_ref()).unwrap();
        assert_eq!(encoded.len(), tx_envelope.encode_2718_len());
        assert_eq!(decoded, tx_envelope);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_serde_roundtrip_deposit() {
        let tx = TxDeposit {
            gas_limit: u64::MAX,
            to: TxKind::Call(Address::random()),
            value: U256::MAX,
            input: Bytes::new(),
            source_hash: U256::MAX.into(),
            from: Address::random(),
            mint: Some(u128::MAX),
            is_system_transaction: false,
        };
        let tx_envelope = OpTxEnvelope::Deposit(tx.seal_slow());

        let serialized = serde_json::to_string(&tx_envelope).unwrap();
        let deserialized: OpTxEnvelope = serde_json::from_str(&serialized).unwrap();

        assert_eq!(tx_envelope, deserialized);
    }

    #[test]
    fn eip2718_deposit_decode() {
        // <https://basescan.org/tx/0xc468b38a20375922828c8126912740105125143b9856936085474b2590bbca91>
        let b = hex!("7ef8f8a0417d134467f4737fcdf2475f0ecdd2a0ed6d87ecffc888ba9f60ee7e3b8ac26a94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e20000008dd00101c1200000000000000040000000066c352bb000000000139c4f500000000000000000000000000000000000000000000000000000000c0cff1460000000000000000000000000000000000000000000000000000000000000001d4c88f4065ac9671e8b1329b90773e89b5ddff9cf8675b2b5e9c1b28320609930000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9");

        let tx = OpTxEnvelope::decode_2718(&mut b[..].as_ref()).unwrap();
        let deposit = tx.as_deposit().unwrap();
        assert!(deposit.mint.is_none());
    }
}
