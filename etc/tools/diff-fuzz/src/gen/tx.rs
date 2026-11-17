//! Transaction construction, signing, and encoding with the workspace transaction types.

use alloy_consensus::{
    SignableTransaction, Signed, TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxLegacy,
};
use alloy_eips::{
    Encodable2718,
    eip2930::{AccessList, AccessListItem},
    eip7702::{Authorization, SignedAuthorization},
};
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use alloy_signer::SignerSync;
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use base_common_evm::BaseSpecId;
use base_common_genesis::BaseUpgrade;
use revm::context_interface::cfg::gas::calculate_initial_tx_gas;

use crate::r#gen::keys::Keys;

/// Transaction envelope shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Legacy, with or without EIP-155 replay protection.
    Legacy {
        /// Whether the chain id is part of the signature.
        eip155: bool,
    },
    /// EIP-2930.
    Eip2930,
    /// EIP-1559.
    Eip1559,
    /// EIP-7702.
    Eip7702,
    /// EIP-4844 (not supported on L2).
    Eip4844,
}

/// A transaction to sign with a generated key.
#[derive(Debug, Clone)]
pub struct TxSpec {
    /// Index of the signing key.
    pub key: usize,
    /// Envelope shape.
    pub shape: Shape,
    /// Chain id.
    pub chain_id: u64,
    /// Nonce.
    pub nonce: u64,
    /// Recipient or creation.
    pub to: TxKind,
    /// Value.
    pub value: U256,
    /// Calldata or init code.
    pub input: Bytes,
    /// Gas limit.
    pub gas_limit: u64,
    /// Max fee per gas (gas price for legacy and EIP-2930).
    pub max_fee: u128,
    /// Max priority fee per gas.
    pub priority_fee: u128,
    /// Access list.
    pub access_list: AccessList,
    /// EIP-7702 authorizations.
    pub authorizations: Vec<SignedAuthorization>,
    /// Replace the signature's `s` with its high-order twin (invalid under EIP-2).
    pub high_s: bool,
}

/// Transaction encoding helpers.
#[derive(Debug)]
pub struct Txs;

impl Txs {
    /// Signs and encodes a transaction.
    pub fn sign(spec: &TxSpec) -> Bytes {
        let signer = &Keys::get().signers[spec.key];
        let to_address = match spec.to {
            TxKind::Call(address) => address,
            TxKind::Create => Address::ZERO,
        };
        let sign = |hash: B256| -> Signature {
            let signature = signer.sign_hash_sync(&hash).expect("signing never fails");
            if spec.high_s {
                let n = U256::from_be_slice(&alloy_primitives::hex!(
                    "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"
                ));
                Signature::new(signature.r(), n - signature.s(), !signature.v())
            } else {
                signature
            }
        };
        let envelope = match spec.shape {
            Shape::Legacy { eip155 } => {
                let tx = TxLegacy {
                    chain_id: eip155.then_some(spec.chain_id),
                    nonce: spec.nonce,
                    gas_price: spec.max_fee,
                    gas_limit: spec.gas_limit,
                    to: spec.to,
                    value: spec.value,
                    input: spec.input.clone(),
                };
                let signature = sign(tx.signature_hash());
                BaseTxEnvelope::Legacy(tx.into_signed(signature))
            }
            Shape::Eip2930 => {
                let tx = TxEip2930 {
                    chain_id: spec.chain_id,
                    nonce: spec.nonce,
                    gas_price: spec.max_fee,
                    gas_limit: spec.gas_limit,
                    to: spec.to,
                    value: spec.value,
                    access_list: spec.access_list.clone(),
                    input: spec.input.clone(),
                };
                let signature = sign(tx.signature_hash());
                BaseTxEnvelope::Eip2930(tx.into_signed(signature))
            }
            Shape::Eip1559 => {
                let tx = TxEip1559 {
                    chain_id: spec.chain_id,
                    nonce: spec.nonce,
                    gas_limit: spec.gas_limit,
                    max_fee_per_gas: spec.max_fee,
                    max_priority_fee_per_gas: spec.priority_fee,
                    to: spec.to,
                    value: spec.value,
                    access_list: spec.access_list.clone(),
                    input: spec.input.clone(),
                };
                let signature = sign(tx.signature_hash());
                BaseTxEnvelope::Eip1559(tx.into_signed(signature))
            }
            Shape::Eip7702 => {
                let tx = TxEip7702 {
                    chain_id: spec.chain_id,
                    nonce: spec.nonce,
                    gas_limit: spec.gas_limit,
                    max_fee_per_gas: spec.max_fee,
                    max_priority_fee_per_gas: spec.priority_fee,
                    to: to_address,
                    value: spec.value,
                    access_list: spec.access_list.clone(),
                    authorization_list: spec.authorizations.clone(),
                    input: spec.input.clone(),
                };
                let signature = sign(tx.signature_hash());
                BaseTxEnvelope::Eip7702(tx.into_signed(signature))
            }
            Shape::Eip4844 => {
                let tx = TxEip4844 {
                    chain_id: spec.chain_id,
                    nonce: spec.nonce,
                    gas_limit: spec.gas_limit,
                    max_fee_per_gas: spec.max_fee,
                    max_priority_fee_per_gas: spec.priority_fee,
                    to: to_address,
                    value: spec.value,
                    access_list: spec.access_list.clone(),
                    blob_versioned_hashes: vec![B256::with_last_byte(1)],
                    max_fee_per_blob_gas: 1,
                    input: spec.input.clone(),
                };
                let signature = sign(tx.signature_hash());
                let signed: Signed<TxEip4844> = tx.into_signed(signature);
                return signed.encoded_2718().into();
            }
        };
        envelope.encoded_2718().into()
    }

    /// Encodes a deposit.
    pub fn deposit(deposit: TxDeposit) -> Bytes {
        BaseTxEnvelope::Deposit(alloy_primitives::Sealed::new(deposit)).encoded_2718().into()
    }

    /// Signs an EIP-7702 authorization with key `key`.
    pub fn authorization(
        key: usize,
        chain_id: u64,
        address: Address,
        nonce: u64,
    ) -> SignedAuthorization {
        let authorization = Authorization { chain_id: U256::from(chain_id), address, nonce };
        let signer = &Keys::get().signers[key];
        let signature =
            signer.sign_hash_sync(&authorization.signature_hash()).expect("signing never fails");
        authorization.into_signed(signature)
    }

    /// An access list over the given addresses, each with `slots` storage keys.
    pub fn access_list(addresses: &[Address], slots: &[B256]) -> AccessList {
        AccessList(
            addresses
                .iter()
                .map(|address| AccessListItem { address: *address, storage_keys: slots.to_vec() })
                .collect(),
        )
    }

    /// Intrinsic gas and the EIP-7623 calldata floor under `upgrade`.
    pub fn intrinsic(
        upgrade: BaseUpgrade,
        input: &[u8],
        create: bool,
        access_list: &AccessList,
        authorizations: usize,
    ) -> (u64, u64) {
        let keys: usize = access_list.0.iter().map(|item| item.storage_keys.len()).sum();
        let gas = calculate_initial_tx_gas(
            BaseSpecId::new(upgrade).into(),
            input,
            create,
            access_list.0.len() as u64,
            keys as u64,
            authorizations as u64,
            None,
        );
        (gas.initial_regular_gas + gas.initial_state_gas, gas.floor_gas)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::SignerRecoverable;
    use alloy_eips::Decodable2718;
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use base_common_consensus::BaseTxEnvelope;
    use base_common_genesis::BaseUpgrade;

    use super::{Shape, TxSpec, Txs};
    use crate::r#gen::keys::Keys;

    #[test]
    fn signed_transactions_recover_their_sender() {
        for shape in [
            Shape::Legacy { eip155: false },
            Shape::Legacy { eip155: true },
            Shape::Eip2930,
            Shape::Eip1559,
            Shape::Eip7702,
        ] {
            let spec = TxSpec {
                key: 3,
                shape,
                chain_id: 8453,
                nonce: 7,
                to: TxKind::Call(Address::repeat_byte(1)),
                value: U256::from(5),
                input: Bytes::from_static(&[1, 2, 3]),
                gas_limit: 50_000,
                max_fee: 1_000_000,
                priority_fee: 10,
                access_list: Txs::access_list(&[Address::repeat_byte(2)], &[]),
                authorizations: vec![Txs::authorization(4, 8453, Address::repeat_byte(9), 0)],
                high_s: false,
            };
            let raw = Txs::sign(&spec);
            let tx = BaseTxEnvelope::decode_2718(&mut raw.as_ref()).unwrap();
            assert_eq!(tx.recover_signer().unwrap(), Keys::get().address(3));
        }
        let (intrinsic, _) =
            Txs::intrinsic(BaseUpgrade::Jovian, &[], false, &Default::default(), 0);
        assert_eq!(intrinsic, 21_000);
    }
}
