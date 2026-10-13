//! The canonical native BLS transfer transaction (EIP-2718 type `0x7b`).

#[cfg(feature = "serde")]
use alloy_consensus::Sealed;
use alloy_consensus::{InMemorySize, Sealable, Transaction};
use alloy_eips::{
    Typed2718,
    eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718, IsTyped2718},
    eip2930::AccessList,
    eip7702::SignedAuthorization,
};
#[cfg(feature = "evm")]
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{Address, B256, Bytes, ChainId, TxHash, TxKind, U256, keccak256};
use alloy_rlp::{BufMut, Decodable, Encodable, Header};
#[cfg(feature = "evm")]
use revm::context::TxEnv;

use crate::BlsConstants;

/// Returned by [`Transaction::input`]: a transfer never carries calldata.
static EMPTY_INPUT: Bytes = Bytes::new();

/// Native ETH transfer authorized by a BLS signature of its sender, EIP-2718 type `0x7b`.
///
/// The canonical encoding, used in blocks and for the transaction hash, is
/// `0x7b || rlp([chain_id, sender, nonce, recipient, value, max_priority_fee_per_gas,
/// max_fee_per_gas])`. It carries no signature: the transaction hash is
/// `keccak256(canonical bytes)` and identifies the transfer intent independently of any witness.
/// The sender signs [`Self::signing_digest`] with BLS; the pooled wire form
/// ([`crate::BlsTransferWithSignature`]) carries that signature.
///
/// The [`Transaction`] projection is an EIP-1559 call from `sender` to `recipient` with
/// [`BlsConstants::TRANSFER_GAS_LIMIT`] gas, empty input and no access, authorization or blob
/// lists.
///
/// The explicit `sender` is not authenticated by this type. Sender recovery returns it unchecked,
/// as for deposits, so admission, block validation and derivation must verify the BLS witness
/// separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "reth", derive(reth_codecs::Compact), reth_codecs(crate = "reth_codecs"))]
pub struct TxBlsTransfer {
    /// EIP-155 chain ID.
    pub chain_id: ChainId,
    /// Account debited by the transfer and charged its fees.
    pub sender: Address,
    /// Sender account nonce consumed by the transfer.
    pub nonce: u64,
    /// Account credited by the transfer.
    pub recipient: Address,
    /// Transferred amount in wei.
    pub value: U256,
    /// Maximum priority fee per gas paid by the sender.
    pub max_priority_fee_per_gas: u128,
    /// Maximum total fee per gas paid by the sender.
    pub max_fee_per_gas: u128,
}

impl TxBlsTransfer {
    /// Returns the transaction hash, `keccak256` of the canonical EIP-2718 encoding.
    pub fn tx_hash(&self) -> TxHash {
        keccak256(self.encoded_2718())
    }

    /// Returns the message the sender signs: `keccak256(BlsConstants::DOMAIN || tx_hash)`.
    ///
    /// The transaction hash commits to the chain ID and to every transfer and fee field.
    pub fn signing_digest(&self) -> B256 {
        Self::signing_digest_for_hash(self.tx_hash())
    }

    /// Returns [`Self::signing_digest`] for an already computed transaction hash, such as the
    /// seal of a sealed transfer.
    pub fn signing_digest_for_hash(tx_hash: TxHash) -> B256 {
        let mut preimage = [0u8; 64];
        preimage[..32].copy_from_slice(BlsConstants::DOMAIN.as_slice());
        preimage[32..].copy_from_slice(tx_hash.as_slice());
        keccak256(preimage)
    }

    /// Returns the RLP payload length of the fields, without the list header.
    pub fn rlp_encoded_fields_length(&self) -> usize {
        self.chain_id.length()
            + self.sender.length()
            + self.nonce.length()
            + self.recipient.length()
            + self.value.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
    }

    /// RLP-encodes the fields in canonical order, without the list header.
    pub fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.chain_id.encode(out);
        self.sender.encode(out);
        self.nonce.encode(out);
        self.recipient.encode(out);
        self.value.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
    }

    /// Decodes the fields in canonical order, without the list header.
    pub fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self {
            chain_id: Decodable::decode(buf)?,
            sender: Decodable::decode(buf)?,
            nonce: Decodable::decode(buf)?,
            recipient: Decodable::decode(buf)?,
            value: Decodable::decode(buf)?,
            max_priority_fee_per_gas: Decodable::decode(buf)?,
            max_fee_per_gas: Decodable::decode(buf)?,
        })
    }

    /// Returns the list header for the canonical intent fields.
    pub fn rlp_header(&self) -> Header {
        Header { list: true, payload_length: self.rlp_encoded_fields_length() }
    }

    /// Deserializes a [`Sealed<TxBlsTransfer>`] by recomputing its hash. Rejects an input whose
    /// optional `hash` field disagrees with the recomputed hash.
    #[cfg(feature = "serde")]
    pub fn deserialize_sealed<'de, D>(deserializer: D) -> Result<Sealed<Self>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct SealedTransfer {
            #[serde(flatten)]
            transaction: TxBlsTransfer,
            #[serde(default)]
            hash: Option<B256>,
        }

        let SealedTransfer { transaction, hash } = serde::Deserialize::deserialize(deserializer)?;
        let sealed = transaction.seal_slow();
        if hash.is_some_and(|hash| hash != sealed.hash()) {
            return Err(serde::de::Error::custom("BLS transfer hash does not match its fields"));
        }
        Ok(sealed)
    }
}

/// Serializes the fields in camelCase plus the fixed `gas` limit, so JSON consumers see the same
/// gas field as for other transaction types.
#[cfg(feature = "serde")]
impl serde::Serialize for TxBlsTransfer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Fields<'a> {
            #[serde(with = "alloy_serde::quantity")]
            chain_id: ChainId,
            sender: &'a Address,
            #[serde(with = "alloy_serde::quantity")]
            nonce: u64,
            recipient: &'a Address,
            value: &'a U256,
            #[serde(with = "alloy_serde::quantity")]
            max_priority_fee_per_gas: u128,
            #[serde(with = "alloy_serde::quantity")]
            max_fee_per_gas: u128,
            #[serde(with = "alloy_serde::quantity")]
            gas: u64,
        }

        Fields {
            chain_id: self.chain_id,
            sender: &self.sender,
            nonce: self.nonce,
            recipient: &self.recipient,
            value: &self.value,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            max_fee_per_gas: self.max_fee_per_gas,
            gas: BlsConstants::TRANSFER_GAS_LIMIT,
        }
        .serialize(serializer)
    }
}

/// Deserializes the camelCase fields. An optional `gas` (or `gasLimit`) field must equal
/// [`BlsConstants::TRANSFER_GAS_LIMIT`].
#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for TxBlsTransfer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Fields {
            #[serde(with = "alloy_serde::quantity")]
            chain_id: ChainId,
            sender: Address,
            #[serde(with = "alloy_serde::quantity")]
            nonce: u64,
            recipient: Address,
            value: U256,
            #[serde(with = "alloy_serde::quantity")]
            max_priority_fee_per_gas: u128,
            #[serde(with = "alloy_serde::quantity")]
            max_fee_per_gas: u128,
            #[serde(default, with = "alloy_serde::quantity::opt", alias = "gasLimit")]
            gas: Option<u64>,
        }

        let fields = Fields::deserialize(deserializer)?;
        if fields.gas.is_some_and(|gas| gas != BlsConstants::TRANSFER_GAS_LIMIT) {
            return Err(serde::de::Error::custom("BLS transfer gas limit must be 21000"));
        }
        Ok(Self {
            chain_id: fields.chain_id,
            sender: fields.sender,
            nonce: fields.nonce,
            recipient: fields.recipient,
            value: fields.value,
            max_priority_fee_per_gas: fields.max_priority_fee_per_gas,
            max_fee_per_gas: fields.max_fee_per_gas,
        })
    }
}

impl Encodable for TxBlsTransfer {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_header().encode(out);
        self.rlp_encode_fields(out);
    }

    fn length(&self) -> usize {
        self.rlp_header().length_with_payload()
    }
}

impl Decodable for TxBlsTransfer {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        if header.payload_length > buf.len() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let remaining = buf.len();
        let this = Self::rlp_decode_fields(buf)?;
        if remaining - buf.len() != header.payload_length {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }
        Ok(this)
    }
}

impl Typed2718 for TxBlsTransfer {
    fn ty(&self) -> u8 {
        BlsConstants::TRANSFER_TX_TYPE
    }
}

impl IsTyped2718 for TxBlsTransfer {
    fn is_type(type_id: u8) -> bool {
        type_id == BlsConstants::TRANSFER_TX_TYPE
    }
}

impl Encodable2718 for TxBlsTransfer {
    fn type_flag(&self) -> Option<u8> {
        Some(BlsConstants::TRANSFER_TX_TYPE)
    }

    fn encode_2718_len(&self) -> usize {
        1 + self.length()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        out.put_u8(BlsConstants::TRANSFER_TX_TYPE);
        self.encode(out);
    }
}

impl Decodable2718 for TxBlsTransfer {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty != BlsConstants::TRANSFER_TX_TYPE {
            return Err(Eip2718Error::UnexpectedType(ty));
        }
        Ok(Self::decode(buf)?)
    }

    fn fallback_decode(_buf: &mut &[u8]) -> Eip2718Result<Self> {
        Err(Eip2718Error::UnexpectedType(0))
    }
}

impl Sealable for TxBlsTransfer {
    fn hash_slow(&self) -> B256 {
        self.tx_hash()
    }
}

impl Transaction for TxBlsTransfer {
    fn chain_id(&self) -> Option<ChainId> {
        Some(self.chain_id)
    }

    fn nonce(&self) -> u64 {
        self.nonce
    }

    fn gas_limit(&self) -> u64 {
        BlsConstants::TRANSFER_GAS_LIMIT
    }

    fn gas_price(&self) -> Option<u128> {
        None
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        Some(self.max_priority_fee_per_gas)
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        None
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.max_priority_fee_per_gas
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        alloy_eips::eip1559::calc_effective_gas_price(
            self.max_fee_per_gas,
            self.max_priority_fee_per_gas,
            base_fee,
        )
    }

    fn is_dynamic_fee(&self) -> bool {
        true
    }

    fn kind(&self) -> TxKind {
        TxKind::Call(self.recipient)
    }

    fn is_create(&self) -> bool {
        false
    }

    fn value(&self) -> U256 {
        self.value
    }

    fn input(&self) -> &Bytes {
        &EMPTY_INPUT
    }

    fn access_list(&self) -> Option<&AccessList> {
        None
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        None
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        None
    }
}

impl InMemorySize for TxBlsTransfer {
    fn size(&self) -> usize {
        size_of::<Self>()
    }
}

#[cfg(feature = "alloy-compat")]
impl From<TxBlsTransfer> for alloy_rpc_types_eth::TransactionRequest {
    fn from(tx: TxBlsTransfer) -> Self {
        Self {
            from: Some(tx.sender),
            to: Some(TxKind::Call(tx.recipient)),
            value: Some(tx.value),
            nonce: Some(tx.nonce),
            gas: Some(BlsConstants::TRANSFER_GAS_LIMIT),
            max_fee_per_gas: Some(tx.max_fee_per_gas),
            max_priority_fee_per_gas: Some(tx.max_priority_fee_per_gas),
            chain_id: Some(tx.chain_id),
            transaction_type: Some(BlsConstants::TRANSFER_TX_TYPE),
            ..Default::default()
        }
    }
}

/// Projects the transfer as a plain value call from its explicit `sender`.
///
/// The `caller` argument is ignored: the transfer names the debited account itself, and recovery
/// of this type only ever returns that same `sender`.
#[cfg(feature = "evm")]
impl FromRecoveredTx<TxBlsTransfer> for TxEnv {
    fn from_recovered_tx(tx: &TxBlsTransfer, _caller: Address) -> Self {
        Self {
            tx_type: BlsConstants::TRANSFER_TX_TYPE,
            caller: tx.sender,
            gas_limit: BlsConstants::TRANSFER_GAS_LIMIT,
            gas_price: tx.max_fee_per_gas,
            kind: TxKind::Call(tx.recipient),
            value: tx.value,
            data: Bytes::new(),
            nonce: tx.nonce,
            chain_id: Some(tx.chain_id),
            gas_priority_fee: Some(tx.max_priority_fee_per_gas),
            ..Default::default()
        }
    }
}

#[cfg(feature = "evm")]
impl FromTxWithEncoded<TxBlsTransfer> for TxEnv {
    fn from_encoded_tx(tx: &TxBlsTransfer, caller: Address, _encoded: Bytes) -> Self {
        Self::from_recovered_tx(tx, caller)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_primitives::address;

    use super::*;

    fn sample_tx() -> TxBlsTransfer {
        TxBlsTransfer {
            chain_id: BlsConstants::CHAIN_ID,
            sender: address!("0x1111111111111111111111111111111111111111"),
            nonce: 7,
            recipient: address!("0x2222222222222222222222222222222222222222"),
            value: U256::from(1_000_000_000_000_000_u64),
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 3_000_000_000,
        }
    }

    /// `0x7b || rlp(list)` of the given fields, built independently of the tx codec.
    fn typed_rlp(fields: &[&dyn Encodable]) -> Vec<u8> {
        let mut payload = Vec::new();
        for field in fields {
            field.encode(&mut payload);
        }
        let mut out = alloc::vec![0x7b];
        Header { list: true, payload_length: payload.len() }.encode(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    fn canonical_bytes(tx: &TxBlsTransfer) -> Vec<u8> {
        typed_rlp(&[
            &tx.chain_id,
            &tx.sender,
            &tx.nonce,
            &tx.recipient,
            &tx.value,
            &tx.max_priority_fee_per_gas,
            &tx.max_fee_per_gas,
        ])
    }

    #[test]
    fn canonical_encoding_and_hash_match_contract() {
        let tx = sample_tx();
        let expected = canonical_bytes(&tx);
        assert_eq!(tx.encoded_2718(), expected);
        assert_eq!(tx.encode_2718_len(), expected.len());
        assert_eq!(tx.tx_hash(), keccak256(&expected));
        assert_eq!(tx.seal_slow().hash(), keccak256(&expected));

        let decoded = TxBlsTransfer::decode_2718(&mut expected.as_slice()).unwrap();
        assert_eq!(decoded, tx);
        assert_eq!(crate::decode_2718_canonical::<TxBlsTransfer>(&expected).unwrap(), tx);
    }

    #[test]
    fn signing_digest_binds_domain_and_every_field() {
        let tx = sample_tx();
        let mut preimage = BlsConstants::DOMAIN.to_vec();
        preimage.extend_from_slice(keccak256(canonical_bytes(&tx)).as_slice());
        assert_eq!(tx.signing_digest(), keccak256(&preimage));
        assert_eq!(TxBlsTransfer::signing_digest_for_hash(tx.tx_hash()), tx.signing_digest());

        let mutations: [fn(&mut TxBlsTransfer); 7] = [
            |tx| tx.chain_id = 8453,
            |tx| tx.sender = Address::repeat_byte(0x33),
            |tx| tx.nonce += 1,
            |tx| tx.recipient = Address::repeat_byte(0x44),
            |tx| tx.value += U256::from(1),
            |tx| tx.max_priority_fee_per_gas += 1,
            |tx| tx.max_fee_per_gas += 1,
        ];
        for (index, mutate) in mutations.iter().enumerate() {
            let mut changed = tx;
            mutate(&mut changed);
            assert_ne!(changed.tx_hash(), tx.tx_hash(), "field {index}");
            assert_ne!(changed.signing_digest(), tx.signing_digest(), "field {index}");
        }
    }

    #[test]
    fn decode_rejects_non_canonical_encodings() {
        let tx = sample_tx();
        let short_address = Bytes::from_static(&[0x11; 19]);
        let leading_zero_nonce = Bytes::from_static(&[0x00, 0x07]);
        let cases: [(&str, Vec<u8>); 5] = [
            (
                "missing field",
                typed_rlp(&[
                    &tx.chain_id,
                    &tx.sender,
                    &tx.nonce,
                    &tx.recipient,
                    &tx.value,
                    &tx.max_priority_fee_per_gas,
                ]),
            ),
            (
                "extra field",
                typed_rlp(&[
                    &tx.chain_id,
                    &tx.sender,
                    &tx.nonce,
                    &tx.recipient,
                    &tx.value,
                    &tx.max_priority_fee_per_gas,
                    &tx.max_fee_per_gas,
                    &0u8,
                ]),
            ),
            (
                "short sender",
                typed_rlp(&[
                    &tx.chain_id,
                    &short_address,
                    &tx.nonce,
                    &tx.recipient,
                    &tx.value,
                    &tx.max_priority_fee_per_gas,
                    &tx.max_fee_per_gas,
                ]),
            ),
            (
                "leading-zero nonce",
                typed_rlp(&[
                    &tx.chain_id,
                    &tx.sender,
                    &leading_zero_nonce,
                    &tx.recipient,
                    &tx.value,
                    &tx.max_priority_fee_per_gas,
                    &tx.max_fee_per_gas,
                ]),
            ),
            ("untyped body", canonical_bytes(&tx)[1..].to_vec()),
        ];
        for (name, bytes) in cases {
            assert!(TxBlsTransfer::decode_2718(&mut bytes.as_slice()).is_err(), "{name}");
        }

        let mut wrong_type = canonical_bytes(&tx);
        wrong_type[0] = BlsConstants::TX_TYPE;
        assert!(TxBlsTransfer::decode_2718(&mut wrong_type.as_slice()).is_err());
    }

    #[test]
    fn transaction_projection_is_fixed_gas_value_call() {
        let tx = sample_tx();
        assert_eq!(tx.ty(), 0x7b);
        assert_eq!(tx.kind(), TxKind::Call(tx.recipient));
        assert_eq!(tx.to(), Some(tx.recipient));
        assert_eq!(tx.value(), tx.value);
        assert_eq!(tx.gas_limit(), 21_000);
        assert!(tx.input().is_empty());
        assert!(tx.access_list().is_none());
        assert!(tx.authorization_list().is_none());
        assert!(tx.blob_versioned_hashes().is_none());
        assert!(tx.is_dynamic_fee());
        // EIP-1559 fee semantics: capped at max_fee, otherwise base fee plus tip.
        assert_eq!(tx.effective_gas_price(Some(2_500_000_000)), 3_000_000_000);
        assert_eq!(tx.effective_gas_price(Some(1_500_000_000)), 2_500_000_000);
    }

    #[cfg(feature = "evm")]
    #[test]
    fn tx_env_debits_explicit_sender() {
        let tx = sample_tx();
        let env = TxEnv::from_recovered_tx(&tx, Address::repeat_byte(0xee));
        assert_eq!(env.tx_type, 0x7b);
        assert_eq!(env.caller, tx.sender);
        assert_eq!(env.kind, TxKind::Call(tx.recipient));
        assert_eq!(env.value, tx.value);
        assert_eq!(env.gas_limit, 21_000);
        assert!(env.data.is_empty());
        assert_eq!(env.nonce, 7);
        assert_eq!(env.chain_id, Some(BlsConstants::CHAIN_ID));
        assert_eq!(env.gas_price, 3_000_000_000);
        assert_eq!(env.gas_priority_fee, Some(1_000_000_000));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_exposes_fixed_gas_and_rejects_other_limits() {
        let tx = sample_tx();
        let mut json = serde_json::to_value(tx).unwrap();
        assert_eq!(json["gas"], "0x5208");
        assert_eq!(json["sender"], "0x1111111111111111111111111111111111111111");
        assert_eq!(json["maxFeePerGas"], "0xb2d05e00");
        assert_eq!(serde_json::from_value::<TxBlsTransfer>(json.clone()).unwrap(), tx);

        json["gas"] = "0x5209".into();
        assert!(serde_json::from_value::<TxBlsTransfer>(json.clone()).is_err());
        json.as_object_mut().unwrap().remove("gas");
        assert_eq!(serde_json::from_value::<TxBlsTransfer>(json).unwrap(), tx);
    }
}
