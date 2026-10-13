//! The unsigned development-only native BLS transaction.

use alloy_consensus::{
    InMemorySize, SignableTransaction, Transaction,
    transaction::{RlpEcdsaDecodableTx, RlpEcdsaEncodableTx},
};
use alloy_eips::{
    Typed2718, eip2718::IsTyped2718, eip2930::AccessList, eip7702::SignedAuthorization,
};
#[cfg(feature = "evm")]
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded};
#[cfg(feature = "evm")]
use alloy_primitives::Address;
use alloy_primitives::{B256, Bytes, ChainId, Signature, TxKind, U256};
use alloy_rlp::{BufMut, Decodable, Encodable};
#[cfg(feature = "evm")]
use revm::context::TxEnv;

use crate::{BlsConstants, BlsPayload, BlsPayloadError};

/// Development-only native BLS transaction, EIP-2718 type `0x7a`.
///
/// The signed wire form is
/// `0x7a || rlp([chain_id, nonce, max_priority_fee_per_gas, max_fee_per_gas, gas_limit, input,
/// y_parity, r, s])`, and the ECDSA signing hash is `keccak256(0x7a || rlp([chain_id, nonce,
/// max_priority_fee_per_gas, max_fee_per_gas, gas_limit, input]))`. The outer ECDSA signer is the
/// registration owner or the batch relayer and pays fees like an EIP-1559 sender.
///
/// The [`Transaction`] projection is a value-free call to [`BlsConstants::REGISTRY_ADDRESS`] with
/// no access list; `input` is a [`BlsPayload`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[cfg_attr(feature = "reth", derive(reth_codecs::Compact), reth_codecs(crate = "reth_codecs"))]
pub struct TxBls {
    /// EIP-155 chain ID.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub chain_id: ChainId,
    /// Outer signer account nonce.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub nonce: u64,
    /// Maximum priority fee per gas paid by the outer signer.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_priority_fee_per_gas: u128,
    /// Maximum total fee per gas paid by the outer signer.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub max_fee_per_gas: u128,
    /// Gas limit.
    #[cfg_attr(
        feature = "serde",
        serde(with = "alloy_serde::quantity", rename = "gas", alias = "gasLimit")
    )]
    pub gas_limit: u64,
    /// Encoded [`BlsPayload`].
    pub input: Bytes,
}

impl TxBls {
    /// Decodes and validates [`Self::input`] as a canonical [`BlsPayload`].
    pub fn payload(&self) -> Result<BlsPayload, BlsPayloadError> {
        BlsPayload::decode(&self.input)
    }
}

impl RlpEcdsaEncodableTx for TxBls {
    fn rlp_encoded_fields_length(&self) -> usize {
        self.chain_id.length()
            + self.nonce.length()
            + self.max_priority_fee_per_gas.length()
            + self.max_fee_per_gas.length()
            + self.gas_limit.length()
            + self.input.0.length()
    }

    fn rlp_encode_fields(&self, out: &mut dyn BufMut) {
        self.chain_id.encode(out);
        self.nonce.encode(out);
        self.max_priority_fee_per_gas.encode(out);
        self.max_fee_per_gas.encode(out);
        self.gas_limit.encode(out);
        self.input.0.encode(out);
    }
}

impl RlpEcdsaDecodableTx for TxBls {
    const DEFAULT_TX_TYPE: u8 = BlsConstants::TX_TYPE;

    fn rlp_decode_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self {
            chain_id: Decodable::decode(buf)?,
            nonce: Decodable::decode(buf)?,
            max_priority_fee_per_gas: Decodable::decode(buf)?,
            max_fee_per_gas: Decodable::decode(buf)?,
            gas_limit: Decodable::decode(buf)?,
            input: Decodable::decode(buf)?,
        })
    }
}

impl Transaction for TxBls {
    fn chain_id(&self) -> Option<ChainId> {
        Some(self.chain_id)
    }

    fn nonce(&self) -> u64 {
        self.nonce
    }

    fn gas_limit(&self) -> u64 {
        self.gas_limit
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
        TxKind::Call(BlsConstants::REGISTRY_ADDRESS)
    }

    fn is_create(&self) -> bool {
        false
    }

    fn value(&self) -> U256 {
        U256::ZERO
    }

    fn input(&self) -> &Bytes {
        &self.input
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

impl Typed2718 for TxBls {
    fn ty(&self) -> u8 {
        BlsConstants::TX_TYPE
    }
}

impl IsTyped2718 for TxBls {
    fn is_type(type_id: u8) -> bool {
        type_id == BlsConstants::TX_TYPE
    }
}

impl SignableTransaction<Signature> for TxBls {
    fn set_chain_id(&mut self, chain_id: ChainId) {
        self.chain_id = chain_id;
    }

    fn encode_for_signing(&self, out: &mut dyn BufMut) {
        out.put_u8(BlsConstants::TX_TYPE);
        self.encode(out);
    }

    fn payload_len_for_signature(&self) -> usize {
        self.length() + 1
    }
}

impl Encodable for TxBls {
    fn encode(&self, out: &mut dyn BufMut) {
        self.rlp_encode(out);
    }

    fn length(&self) -> usize {
        self.rlp_encoded_length()
    }
}

impl Decodable for TxBls {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Self::rlp_decode(buf)
    }
}

impl InMemorySize for TxBls {
    fn size(&self) -> usize {
        size_of::<Self>() + self.input.len()
    }
}

#[cfg(feature = "alloy-compat")]
impl From<TxBls> for alloy_rpc_types_eth::TransactionRequest {
    fn from(tx: TxBls) -> Self {
        Self {
            to: Some(TxKind::Call(BlsConstants::REGISTRY_ADDRESS)),
            nonce: Some(tx.nonce),
            gas: Some(tx.gas_limit),
            max_fee_per_gas: Some(tx.max_fee_per_gas),
            max_priority_fee_per_gas: Some(tx.max_priority_fee_per_gas),
            input: alloy_rpc_types_eth::TransactionInput::new(tx.input),
            chain_id: Some(tx.chain_id),
            transaction_type: Some(BlsConstants::TX_TYPE),
            ..Default::default()
        }
    }
}

/// Projects the transaction as a value-free call to the registry with the payload as calldata.
/// Execution must intercept `tx_type == 0x7a` and apply the native registry semantics.
#[cfg(feature = "evm")]
impl FromRecoveredTx<TxBls> for TxEnv {
    fn from_recovered_tx(tx: &TxBls, caller: Address) -> Self {
        Self {
            tx_type: BlsConstants::TX_TYPE,
            caller,
            gas_limit: tx.gas_limit,
            gas_price: tx.max_fee_per_gas,
            kind: TxKind::Call(BlsConstants::REGISTRY_ADDRESS),
            value: U256::ZERO,
            data: tx.input.clone(),
            nonce: tx.nonce,
            chain_id: Some(tx.chain_id),
            gas_priority_fee: Some(tx.max_priority_fee_per_gas),
            ..Default::default()
        }
    }
}

#[cfg(feature = "evm")]
impl FromTxWithEncoded<TxBls> for TxEnv {
    fn from_encoded_tx(tx: &TxBls, caller: Address, _encoded: Bytes) -> Self {
        Self::from_recovered_tx(tx, caller)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_consensus::Signed;
    use alloy_eips::eip2718::{Decodable2718, Encodable2718};
    use alloy_primitives::keccak256;
    use alloy_rlp::Header;
    #[cfg(feature = "k256")]
    use alloy_signer::SignerSync;
    #[cfg(feature = "k256")]
    use alloy_signer_local::PrivateKeySigner;

    use super::*;

    fn registration_input() -> Bytes {
        let mut input = alloc::vec![BlsConstants::KIND_REGISTRATION, 0xa5];
        input.extend_from_slice(&[0x11; 95]);
        input.push(0x95);
        input.extend_from_slice(&[0x11; 47]);
        input.into()
    }

    fn sample_tx() -> TxBls {
        TxBls {
            chain_id: BlsConstants::CHAIN_ID,
            nonce: 7,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 2_000_000_000,
            gas_limit: 100_000,
            input: registration_input(),
        }
    }

    /// RLP list of the given pre-encoded fields, built independently of the tx codec.
    fn rlp_list(fields: &[&dyn Encodable]) -> Vec<u8> {
        let mut payload = Vec::new();
        for field in fields {
            field.encode(&mut payload);
        }
        let mut out = Vec::new();
        Header { list: true, payload_length: payload.len() }.encode(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    #[test]
    fn wire_layout_matches_contract() {
        let tx = sample_tx();
        let signature = Signature::new(U256::from(0x1234), U256::from(0x5678), true);
        let signed = tx.clone().into_signed(signature);

        let mut expected_unsigned = alloc::vec![0x7a];
        expected_unsigned.extend(rlp_list(&[
            &tx.chain_id,
            &tx.nonce,
            &tx.max_priority_fee_per_gas,
            &tx.max_fee_per_gas,
            &tx.gas_limit,
            &tx.input,
        ]));
        assert_eq!(tx.signature_hash(), keccak256(&expected_unsigned));

        let mut expected_signed = alloc::vec![0x7a];
        expected_signed.extend(rlp_list(&[
            &tx.chain_id,
            &tx.nonce,
            &tx.max_priority_fee_per_gas,
            &tx.max_fee_per_gas,
            &tx.gas_limit,
            &tx.input,
            &true,
            &U256::from(0x1234),
            &U256::from(0x5678),
        ]));
        let encoded = signed.encoded_2718();
        assert_eq!(encoded, expected_signed);
        assert_eq!(*signed.hash(), keccak256(&expected_signed));

        let decoded = Signed::<TxBls>::decode_2718(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded, signed);
        assert_eq!(decoded.hash(), signed.hash());
    }

    #[test]
    fn decode_rejects_non_canonical_wire() {
        let tx = sample_tx();
        let fields: [&dyn Encodable; 6] = [
            &tx.chain_id,
            &tx.nonce,
            &tx.max_priority_fee_per_gas,
            &tx.max_fee_per_gas,
            &tx.gas_limit,
            &tx.input,
        ];
        let typed = |extra: &[&dyn Encodable]| {
            let mut all: Vec<&dyn Encodable> = fields.to_vec();
            all.extend_from_slice(extra);
            [&[0x7a][..], &rlp_list(&all)].concat()
        };
        let (r, s) = (U256::from(1), U256::from(2));

        // Valid control.
        assert!(Signed::<TxBls>::decode_2718(&mut typed(&[&1u8, &r, &s]).as_slice()).is_ok());
        // Legacy-style `v` instead of a y-parity bit.
        assert!(Signed::<TxBls>::decode_2718(&mut typed(&[&27u8, &r, &s]).as_slice()).is_err());
        // Missing and extra fields.
        assert!(Signed::<TxBls>::decode_2718(&mut typed(&[&1u8, &r]).as_slice()).is_err());
        assert!(
            Signed::<TxBls>::decode_2718(&mut typed(&[&1u8, &r, &s, &0u8]).as_slice()).is_err()
        );
        // Wrong type byte.
        let mut wrong_type = typed(&[&1u8, &r, &s]);
        wrong_type[0] = 0x02;
        assert!(Signed::<TxBls>::decode_2718(&mut wrong_type.as_slice()).is_err());
        // A chain ID with a leading zero byte is not a canonical RLP integer.
        let leading_zero_chain_id = Bytes::from_static(&[0x00, 0x01]);
        let mut non_canonical: Vec<&dyn Encodable> = alloc::vec![&leading_zero_chain_id];
        non_canonical.extend_from_slice(&fields[1..]);
        non_canonical.extend_from_slice(&[&1u8, &r, &s]);
        let non_canonical = [&[0x7a][..], &rlp_list(&non_canonical)].concat();
        assert!(Signed::<TxBls>::decode_2718(&mut non_canonical.as_slice()).is_err());
    }

    #[test]
    fn transaction_projection_is_value_free_registry_call() {
        let tx = sample_tx();
        assert_eq!(tx.kind(), TxKind::Call(BlsConstants::REGISTRY_ADDRESS));
        assert_eq!(tx.to(), Some(BlsConstants::REGISTRY_ADDRESS));
        assert_eq!(tx.value(), U256::ZERO);
        assert!(tx.access_list().is_none());
        assert!(tx.authorization_list().is_none());
        assert_eq!(tx.input(), &registration_input());
        assert_eq!(tx.ty(), 0x7a);
        // EIP-1559 fee semantics: capped at max_fee, tip above base fee otherwise.
        assert_eq!(tx.effective_gas_price(Some(1_500_000_000)), 2_000_000_000);
        assert_eq!(tx.effective_gas_price(Some(500_000_000)), 1_500_000_000);
        assert!(matches!(tx.payload(), Ok(BlsPayload::Registration { .. })));
    }

    #[cfg(feature = "k256")]
    #[test]
    fn outer_signer_recovers_from_signing_hash() {
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x42)).unwrap();
        let tx = sample_tx();
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let signed = tx.into_signed(signature);
        let encoded = signed.encoded_2718();
        let decoded = Signed::<TxBls>::decode_2718(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded.recover_signer().unwrap(), signer.address());

        // Any field change moves the signing hash, so the same signature recovers someone else.
        let mut tampered = decoded.tx().clone();
        tampered.nonce += 1;
        let tampered = tampered.into_signed(*decoded.signature());
        assert_ne!(tampered.recover_signer().ok(), Some(signer.address()));
    }
}
