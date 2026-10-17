//! Fixtures for building signed candidate submissions in tests.

use alloy_consensus::{SignableTransaction, TxEip1559, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{BaseTransactionSigned, BaseTypedTransaction};

use crate::{
    CandidateEnvelope, CandidateGroup, CandidateLimits, CandidateSelectionRule, CandidateValidator,
    RULE_MAX_RECIPIENT_BALANCE_DELTA, SCORE_VERSION_V1,
};

/// Default gas limit for fixture siblings.
pub const FIXTURE_GAS_LIMIT: u64 = 100_000;

/// Default fee cap for fixture siblings.
pub const FIXTURE_MAX_FEE_PER_GAS: u128 = 1_000_000_000;

/// A sender key plus helpers that sign siblings and envelopes with it.
#[derive(Debug, Clone)]
pub struct CandidateFixture {
    /// Sender key.
    pub signer: PrivateKeySigner,
    /// Chain ID for siblings and the envelope domain.
    pub chain_id: u64,
}

impl CandidateFixture {
    /// Creates a fixture with a fixed default key.
    pub fn new(chain_id: u64) -> Self {
        Self::with_key(chain_id, 0x42)
    }

    /// Creates a fixture whose private key is `byte` repeated.
    pub fn with_key(chain_id: u64, byte: u8) -> Self {
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(byte)).expect("valid key");
        Self { signer, chain_id }
    }

    /// Returns the sender address.
    pub const fn address(&self) -> Address {
        self.signer.address()
    }

    /// Returns an unsigned EIP-1559 sibling with the fixture's uniform fee and gas fields.
    pub fn sibling_tx(&self, nonce: u64, to: Address, value: U256) -> TxEip1559 {
        TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            gas_limit: FIXTURE_GAS_LIMIT,
            max_fee_per_gas: FIXTURE_MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(to),
            value,
            ..Default::default()
        }
    }

    /// Signs and EIP-2718 encodes a sibling.
    pub fn encode(&self, tx: TxEip1559) -> Bytes {
        let signature = self.signer.sign_hash_sync(&tx.signature_hash()).expect("signs");
        BaseTransactionSigned::new_unhashed(BaseTypedTransaction::Eip1559(tx), signature)
            .encoded_2718()
            .into()
    }

    /// Returns an encoded legacy transaction, which candidate groups reject.
    pub fn legacy_sibling(&self, nonce: u64) -> Bytes {
        let tx = TxLegacy {
            chain_id: Some(self.chain_id),
            nonce,
            gas_limit: FIXTURE_GAS_LIMIT,
            gas_price: FIXTURE_MAX_FEE_PER_GAS,
            to: TxKind::Call(Address::repeat_byte(0x77)),
            ..Default::default()
        };
        let signature = self.signer.sign_hash_sync(&tx.signature_hash()).expect("signs");
        BaseTransactionSigned::new_unhashed(BaseTypedTransaction::Legacy(tx), signature)
            .encoded_2718()
            .into()
    }

    /// Returns an envelope for `raw` with permissive defaults: native-asset rule measuring the
    /// sender, `DEFAULT_SIBLING` at `default_index`, expiry block 100.
    pub fn envelope_for(&self, raw: &[Bytes], default_index: u8) -> CandidateEnvelope {
        CandidateEnvelope {
            sender: self.address(),
            nonce: 0,
            sibling_hashes: raw.iter().map(keccak256).collect(),
            rule: CandidateSelectionRule {
                kind: RULE_MAX_RECIPIENT_BALANCE_DELTA,
                score_version: SCORE_VERSION_V1,
                token: Address::ZERO,
                recipient: self.address(),
                min_delta: U256::ZERO,
            },
            fallback_mode: 1,
            default_index,
            max_work_gas: 10_000_000,
            max_evaluations: 3,
            max_access_charge: U256::ZERO,
            expiry_block: 100,
            disclosure: 0,
        }
    }

    /// Signs the envelope's EIP-712 hash.
    pub fn sign(&self, envelope: &CandidateEnvelope) -> Signature {
        self.signer.sign_hash_sync(&envelope.signing_hash(self.chain_id)).expect("signs")
    }

    /// Returns a default envelope for `raw` and its signature.
    pub fn sign_for(&self, raw: &[Bytes], default_index: u8) -> (CandidateEnvelope, Signature) {
        let envelope = self.envelope_for(raw, default_index);
        let signature = self.sign(&envelope);
        (envelope, signature)
    }

    /// Returns `count` nonce-0 siblings to distinct recipients with a signed default envelope.
    pub fn submission(
        &self,
        count: usize,
        default_index: u8,
    ) -> (Vec<Bytes>, CandidateEnvelope, Signature) {
        let raw: Vec<Bytes> = (0..count)
            .map(|index| {
                let to = Address::repeat_byte(u8::try_from(index + 1).expect("small count"));
                self.encode(self.sibling_tx(0, to, U256::ZERO))
            })
            .collect();
        let (envelope, signature) = self.sign_for(&raw, default_index);
        (raw, envelope, signature)
    }

    /// Validates `raw` under `envelope` without an expiry window, panicking on rejection.
    pub fn group(&self, raw: &[Bytes], envelope: CandidateEnvelope) -> CandidateGroup {
        let signature = self.sign(&envelope);
        CandidateValidator { limits: CandidateLimits::default(), chain_id: self.chain_id }
            .validate(raw, envelope, signature, None)
            .expect("valid fixture group")
    }
}
