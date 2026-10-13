//! The pooled wire form of a native BLS transfer, carrying its BLS signature.

use alloy_consensus::{InMemorySize, Sealable, Sealed, Transaction};
use alloy_eips::{
    Typed2718,
    eip2718::{Decodable2718, Eip2718Error, Eip2718Result, Encodable2718, IsTyped2718},
    eip2930::AccessList,
    eip7702::SignedAuthorization,
};
use alloy_primitives::{B256, Bytes, ChainId, FixedBytes, TxHash, TxKind, U256};
use alloy_rlp::{BufMut, Decodable, Encodable, Header};

use crate::{BlsConstants, BlsPayload, BlsPayloadError, TxBlsTransfer};

/// A [`TxBlsTransfer`] with the sender's compressed G1 BLS signature over
/// [`TxBlsTransfer::signing_digest`].
///
/// This is the pooled (`eth_sendRawTransaction` and p2p) wire form:
/// `0x7b || rlp([rlp([chain_id, sender, nonce, recipient, value, max_priority_fee_per_gas,
/// max_fee_per_gas]), signature48])`. The nested list is byte-identical to the canonical encoding
/// after its type byte, so the canonical and pooled forms never decode as each other.
///
/// The transaction hash is the intent hash of [`Self::transaction`]; it excludes the signature,
/// so replacing the witness never changes the hash. Converting to the canonical form drops the
/// signature.
///
/// Decoding rejects a missing or wrong-length signature and any signature that is not a
/// canonical compressed point (compression flag clear, infinity flag set, or coordinate not below
/// the field modulus). It does not check curve or subgroup membership or verify the signature;
/// execution and admission must run full BLS verification.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct BlsTransferWithSignature {
    /// The canonical transfer and its intent hash.
    pub transaction: Sealed<TxBlsTransfer>,
    /// Compressed G1 BLS signature of the sender over [`TxBlsTransfer::signing_digest`].
    pub signature: FixedBytes<48>,
}

impl BlsTransferWithSignature {
    /// Pairs a transfer with its signature after checking that the signature is a canonical,
    /// non-infinity compressed G1 encoding.
    pub fn new(
        transaction: Sealed<TxBlsTransfer>,
        signature: FixedBytes<48>,
    ) -> Result<Self, BlsPayloadError> {
        if !BlsPayload::is_canonical_point(signature.as_slice()) {
            return Err(BlsPayloadError::NonCanonicalSignature);
        }
        Ok(Self { transaction, signature })
    }

    /// Returns the intent hash, which excludes the signature.
    pub const fn hash(&self) -> &TxHash {
        self.transaction.hash_ref()
    }

    /// Returns the message the signature must verify against.
    pub fn signing_digest(&self) -> B256 {
        TxBlsTransfer::signing_digest_for_hash(self.transaction.hash())
    }

    /// Returns the list header for the pooled intent and detached witness.
    pub fn rlp_header(&self) -> Header {
        Header { list: true, payload_length: self.transaction.length() + self.signature.length() }
    }
}

impl From<BlsTransferWithSignature> for Sealed<TxBlsTransfer> {
    fn from(value: BlsTransferWithSignature) -> Self {
        value.transaction
    }
}

impl Typed2718 for BlsTransferWithSignature {
    fn ty(&self) -> u8 {
        BlsConstants::TRANSFER_TX_TYPE
    }
}

impl IsTyped2718 for BlsTransferWithSignature {
    fn is_type(type_id: u8) -> bool {
        type_id == BlsConstants::TRANSFER_TX_TYPE
    }
}

impl Encodable2718 for BlsTransferWithSignature {
    fn type_flag(&self) -> Option<u8> {
        Some(BlsConstants::TRANSFER_TX_TYPE)
    }

    fn encode_2718_len(&self) -> usize {
        1 + self.rlp_header().length_with_payload()
    }

    fn encode_2718(&self, out: &mut dyn BufMut) {
        out.put_u8(BlsConstants::TRANSFER_TX_TYPE);
        self.rlp_header().encode(out);
        self.transaction.encode(out);
        self.signature.encode(out);
    }

    /// The intent hash, not the hash of the pooled bytes.
    fn trie_hash(&self) -> B256 {
        self.transaction.hash()
    }
}

impl Decodable2718 for BlsTransferWithSignature {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty != BlsConstants::TRANSFER_TX_TYPE {
            return Err(Eip2718Error::UnexpectedType(ty));
        }
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString.into());
        }
        if header.payload_length > buf.len() {
            return Err(alloy_rlp::Error::InputTooShort.into());
        }
        let remaining = buf.len();
        let transaction = TxBlsTransfer::decode(buf)?;
        let signature = FixedBytes::<48>::decode(buf)?;
        if remaining - buf.len() != header.payload_length {
            return Err(alloy_rlp::Error::UnexpectedLength.into());
        }
        Self::new(transaction.seal_slow(), signature)
            .map_err(|_| alloy_rlp::Error::Custom("non-canonical compressed BLS signature").into())
    }

    fn fallback_decode(_buf: &mut &[u8]) -> Eip2718Result<Self> {
        Err(Eip2718Error::UnexpectedType(0))
    }
}

impl Transaction for BlsTransferWithSignature {
    fn chain_id(&self) -> Option<ChainId> {
        self.transaction.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.transaction.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.transaction.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.transaction.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.transaction.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.transaction.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.transaction.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.transaction.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.transaction.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.transaction.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.transaction.kind()
    }

    fn is_create(&self) -> bool {
        self.transaction.is_create()
    }

    fn value(&self) -> U256 {
        self.transaction.value()
    }

    fn input(&self) -> &Bytes {
        self.transaction.input()
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.transaction.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.transaction.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        self.transaction.authorization_list()
    }
}

impl InMemorySize for BlsTransferWithSignature {
    fn size(&self) -> usize {
        size_of::<Self>()
    }
}

/// Serializes the transfer fields, `hash` and `signature` as one flat object.
#[cfg(feature = "serde")]
impl serde::Serialize for BlsTransferWithSignature {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Flat<'a> {
            #[serde(flatten)]
            transaction: &'a Sealed<TxBlsTransfer>,
            signature: &'a FixedBytes<48>,
        }

        Flat { transaction: &self.transaction, signature: &self.signature }.serialize(serializer)
    }
}

/// Deserializes the flat object written by [`serde::Serialize`], recomputing the hash and
/// applying the same signature encoding checks as [`Self::new`].
#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for BlsTransferWithSignature {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Flat {
            #[serde(flatten, deserialize_with = "TxBlsTransfer::deserialize_sealed")]
            transaction: Sealed<TxBlsTransfer>,
            signature: FixedBytes<48>,
        }

        let flat = Flat::deserialize(deserializer)?;
        Self::new(flat.transaction, flat.signature).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_primitives::{Address, keccak256};

    use super::*;
    use crate::{BasePooledTransaction, BaseTxEnvelope};

    fn sample_tx() -> TxBlsTransfer {
        TxBlsTransfer {
            chain_id: BlsConstants::CHAIN_ID,
            sender: Address::repeat_byte(0x11),
            nonce: 3,
            recipient: Address::repeat_byte(0x22),
            value: U256::from(5u64),
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 2,
        }
    }

    /// A canonical compressed G1 encoding (compression and sign flags set, coordinate below p).
    fn signature(fill: u8) -> FixedBytes<48> {
        let mut bytes = [fill; 48];
        bytes[0] = 0x80 | 0x20 | 0x11;
        bytes.into()
    }

    /// `0x7b || rlp([canonical fields list, signature-ish])`, built independently of the codec.
    fn pooled_bytes(tx: &TxBlsTransfer, rest: &[&dyn Encodable]) -> Vec<u8> {
        let canonical = tx.encoded_2718();
        let mut payload = canonical[1..].to_vec();
        for item in rest {
            item.encode(&mut payload);
        }
        let mut out = alloc::vec![0x7b];
        Header { list: true, payload_length: payload.len() }.encode(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    fn decode(bytes: &[u8]) -> Eip2718Result<BlsTransferWithSignature> {
        BlsTransferWithSignature::decode_2718(&mut &bytes[..])
    }

    #[test]
    fn pooled_encoding_matches_contract() {
        let tx = sample_tx();
        let pooled = BlsTransferWithSignature::new(tx.seal_slow(), signature(0x11)).unwrap();
        let expected = pooled_bytes(&tx, &[&signature(0x11)]);
        assert_eq!(pooled.encoded_2718(), expected);
        assert_eq!(pooled.encode_2718_len(), expected.len());
        assert_eq!(decode(&expected).unwrap(), pooled);
        assert_eq!(*pooled.hash(), keccak256(tx.encoded_2718()));
        assert_eq!(pooled.trie_hash(), *pooled.hash());
        assert_eq!(pooled.signing_digest(), tx.signing_digest());
    }

    #[test]
    fn intent_hash_is_stable_when_witness_changes() {
        let tx = sample_tx();
        let first = BasePooledTransaction::decode_2718(
            &mut pooled_bytes(&tx, &[&signature(0x11)]).as_slice(),
        )
        .unwrap();
        let second = BasePooledTransaction::decode_2718(
            &mut pooled_bytes(&tx, &[&signature(0x05)]).as_slice(),
        )
        .unwrap();
        assert_ne!(first.encoded_2718(), second.encoded_2718());
        assert_eq!(first.hash(), second.hash());
        assert_eq!(*first.hash(), tx.tx_hash());

        // Dropping the witness yields the same canonical transaction from either form.
        let canonical = BaseTxEnvelope::from(first);
        assert_eq!(canonical, BaseTxEnvelope::from(second));
        assert_eq!(canonical.encoded_2718(), tx.encoded_2718());
        assert_eq!(canonical.tx_hash(), tx.tx_hash());
    }

    #[test]
    fn canonical_and_pooled_forms_do_not_decode_as_each_other() {
        let tx = sample_tx();
        let canonical = tx.encoded_2718();
        let pooled = pooled_bytes(&tx, &[&signature(0x11)]);

        assert!(decode(&canonical).is_err());
        assert!(BasePooledTransaction::decode_2718(&mut canonical.as_slice()).is_err());
        assert!(TxBlsTransfer::decode_2718(&mut pooled.as_slice()).is_err());
        assert!(BaseTxEnvelope::decode_2718(&mut pooled.as_slice()).is_err());

        assert!(BaseTxEnvelope::decode_2718(&mut canonical.as_slice()).unwrap().is_bls_transfer());
        assert!(
            BasePooledTransaction::decode_2718(&mut pooled.as_slice()).unwrap().is_bls_transfer()
        );
    }

    #[test]
    fn rejects_missing_malformed_and_non_canonical_signatures() {
        let tx = sample_tx();
        let short = Bytes::from(signature(0x11)[..47].to_vec());
        let long = Bytes::from([signature(0x11).as_slice(), &[0x11]].concat());
        let mut infinity = [0u8; 48];
        infinity[0] = 0xc0;
        let mut uncompressed = signature(0x11);
        uncompressed[0] &= 0x7f;
        let mut modulus = BlsConstants::FIELD_MODULUS;
        modulus[0] |= 0x80;
        let mut below_modulus = modulus;
        below_modulus[47] -= 1;
        let as_list: Vec<FixedBytes<48>> = alloc::vec![signature(0x11)];

        let cases: [(&str, Vec<u8>); 8] = [
            ("missing", pooled_bytes(&tx, &[])),
            ("47 bytes", pooled_bytes(&tx, &[&short])),
            ("49 bytes", pooled_bytes(&tx, &[&long])),
            ("list", pooled_bytes(&tx, &[&as_list])),
            ("trailing field", pooled_bytes(&tx, &[&signature(0x11), &0u8])),
            ("infinity", pooled_bytes(&tx, &[&FixedBytes::from(infinity)])),
            ("uncompressed", pooled_bytes(&tx, &[&uncompressed])),
            ("coordinate equal to p", pooled_bytes(&tx, &[&FixedBytes::from(modulus)])),
        ];
        for (name, bytes) in cases {
            assert!(decode(&bytes).is_err(), "{name}");
            assert!(BasePooledTransaction::decode_2718(&mut bytes.as_slice()).is_err(), "{name}");
        }
        assert!(decode(&pooled_bytes(&tx, &[&FixedBytes::from(below_modulus)])).is_ok());
        assert_eq!(
            BlsTransferWithSignature::new(tx.seal_slow(), infinity.into()),
            Err(BlsPayloadError::NonCanonicalSignature)
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_roundtrip_recomputes_hash_and_checks_signature() {
        let pooled =
            BlsTransferWithSignature::new(sample_tx().seal_slow(), signature(0x11)).unwrap();
        let mut json = serde_json::to_value(&pooled).unwrap();
        assert_eq!(json["hash"], serde_json::to_value(pooled.hash()).unwrap());
        assert_eq!(json["gas"], "0x5208");
        assert_eq!(
            serde_json::from_value::<BlsTransferWithSignature>(json.clone()).unwrap(),
            pooled
        );

        let mut forged = json.clone();
        forged["hash"] = serde_json::to_value(B256::repeat_byte(0xab)).unwrap();
        assert!(serde_json::from_value::<BlsTransferWithSignature>(forged).is_err());

        let mut infinity = [0u8; 48];
        infinity[0] = 0xc0;
        json["signature"] = serde_json::to_value(FixedBytes::from(infinity)).unwrap();
        assert!(serde_json::from_value::<BlsTransferWithSignature>(json).is_err());
    }
}
