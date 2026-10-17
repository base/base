//! Stateless admission checks for a candidate submission (design §2.3 steps 3–5, §4.2).

use alloy_consensus::{
    Transaction,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{B256, Bytes, Signature};
use base_common_consensus::BaseTransactionSigned;

use crate::{
    CandidateEnvelope, CandidateError, CandidateGroup, CandidateSibling, MAX_SIBLINGS_CEILING,
    RULE_MAX_RECIPIENT_BALANCE_DELTA, SCORE_VERSION_V1,
};

/// Default maximum siblings per group; a provisional engineering cap (design §8.3).
pub const DEFAULT_MAX_SIBLINGS: usize = 4;

/// Default maximum encoded size of one sibling, matching reth's default pooled size cap.
pub const DEFAULT_MAX_SIBLING_BYTES: usize = 128 * 1024;

/// Default maximum encoded size of all siblings in one request.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 512 * 1024;

/// Default per-sibling gas-limit cap (design TBD; above the 4.65M Smart Settlement p99).
pub const DEFAULT_MAX_SIBLING_GAS: u64 = 5_000_000;

/// Static size and gas bounds applied at ingress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateLimits {
    /// Maximum siblings per group; at most [`MAX_SIBLINGS_CEILING`].
    pub max_siblings: usize,
    /// Maximum encoded size of one sibling.
    pub max_sibling_bytes: usize,
    /// Maximum encoded size of all siblings in a request.
    pub max_request_bytes: usize,
    /// Maximum shared sibling gas limit.
    pub max_sibling_gas: u64,
}

impl Default for CandidateLimits {
    fn default() -> Self {
        Self {
            max_siblings: DEFAULT_MAX_SIBLINGS,
            max_sibling_bytes: DEFAULT_MAX_SIBLING_BYTES,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_sibling_gas: DEFAULT_MAX_SIBLING_GAS,
        }
    }
}

/// The accepted `expiryBlock` range, measured from the block being built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpiryWindow {
    /// The next block to be built; `expiryBlock` must be at least this.
    pub next_block: u64,
    /// How far past `next_block` `expiryBlock` may be.
    pub max_blocks: u64,
}

impl ExpiryWindow {
    /// Returns `true` when `expiry_block` is inside the window (both ends inclusive).
    pub const fn contains(&self, expiry_block: u64) -> bool {
        expiry_block >= self.next_block
            && expiry_block <= self.next_block.saturating_add(self.max_blocks)
    }
}

/// Validates untrusted candidate submissions into [`CandidateGroup`]s.
#[derive(Debug, Clone, Copy)]
pub struct CandidateValidator {
    /// Bounds applied to every submission.
    pub limits: CandidateLimits,
    /// The only chain ID siblings may carry.
    pub chain_id: u64,
}

impl CandidateValidator {
    /// Runs bounds, decode, uniformity, envelope, expiry and signature checks, in that order.
    ///
    /// `expiry` is `None` before a canonical head exists; `expiryBlock` is still required to be
    /// finite, which a `u64` always is.
    pub fn validate(
        &self,
        raw_siblings: &[Bytes],
        envelope: CandidateEnvelope,
        signature: Signature,
        expiry: Option<ExpiryWindow>,
    ) -> Result<CandidateGroup, CandidateError> {
        self.check_bounds(raw_siblings)?;
        let siblings = Self::decode(raw_siblings)?;
        self.check_uniformity(&siblings)?;
        Self::check_envelope(&envelope, &siblings)?;
        if let Some(window) = expiry
            && !window.contains(envelope.expiry_block)
        {
            return Err(CandidateError::ExpiryOutOfWindow);
        }
        let group = CandidateGroup::new(envelope, signature, self.chain_id, siblings);
        Self::check_signature(&group)?;
        Ok(group)
    }

    fn check_bounds(&self, raw_siblings: &[Bytes]) -> Result<(), CandidateError> {
        let max = self.limits.max_siblings.min(MAX_SIBLINGS_CEILING);
        if raw_siblings.len() < 2 {
            return Err(CandidateError::TooFewSiblings);
        }
        if raw_siblings.len() > max {
            return Err(CandidateError::TooManySiblings { max });
        }
        let mut total = 0_usize;
        for raw in raw_siblings {
            if raw.len() > self.limits.max_sibling_bytes {
                return Err(CandidateError::RequestTooLarge);
            }
            total = total.saturating_add(raw.len());
        }
        if total > self.limits.max_request_bytes {
            return Err(CandidateError::RequestTooLarge);
        }
        Ok(())
    }

    fn decode(raw_siblings: &[Bytes]) -> Result<Vec<CandidateSibling>, CandidateError> {
        raw_siblings
            .iter()
            .enumerate()
            .map(|(index, raw)| {
                let tx = BaseTransactionSigned::decode_2718_exact(raw.as_ref())
                    .map_err(|_| CandidateError::SiblingDecodeFailed { index })?;
                if !matches!(tx, BaseTransactionSigned::Eip1559(_)) {
                    return Err(CandidateError::SiblingTypeUnsupported { index });
                }
                let signer = tx
                    .recover_signer()
                    .map_err(|_| CandidateError::SiblingDecodeFailed { index })?;
                Ok(CandidateSibling {
                    transaction: Recovered::new_unchecked(tx, signer),
                    encoded: raw.clone(),
                })
            })
            .collect()
    }

    fn check_uniformity(&self, siblings: &[CandidateSibling]) -> Result<(), CandidateError> {
        let first = &siblings[0];
        let first_tx = first.transaction.inner();
        for sibling in siblings {
            let tx = sibling.transaction.inner();
            if sibling.transaction.signer() != first.transaction.signer() {
                return Err(CandidateError::SiblingSenderMismatch);
            }
            if tx.nonce() != first_tx.nonce()
                || tx.chain_id() != first_tx.chain_id()
                || tx.chain_id() != Some(self.chain_id)
            {
                return Err(CandidateError::SiblingNonceMismatch);
            }
            if tx.max_fee_per_gas() != first_tx.max_fee_per_gas()
                || tx.max_priority_fee_per_gas() != first_tx.max_priority_fee_per_gas()
            {
                return Err(CandidateError::SiblingFeeMismatch);
            }
            if tx.gas_limit() != first_tx.gas_limit() {
                return Err(CandidateError::SiblingGasLimitMismatch);
            }
        }
        if first_tx.gas_limit() > self.limits.max_sibling_gas {
            return Err(CandidateError::SiblingGasLimitTooHigh {
                max: self.limits.max_sibling_gas,
            });
        }
        for (index, sibling) in siblings.iter().enumerate() {
            let hash = sibling.hash();
            if siblings[..index].iter().any(|earlier| earlier.hash() == hash) {
                return Err(CandidateError::DuplicateSibling);
            }
        }
        Ok(())
    }

    fn check_envelope(
        envelope: &CandidateEnvelope,
        siblings: &[CandidateSibling],
    ) -> Result<(), CandidateError> {
        let hashes = siblings.iter().map(CandidateSibling::hash);
        if envelope.sibling_hashes.len() != siblings.len()
            || !envelope.sibling_hashes.iter().copied().eq(hashes)
        {
            return Err(CandidateError::EnvelopeHashMismatch);
        }
        let first = &siblings[0];
        if envelope.sender != first.transaction.signer() {
            return Err(CandidateError::EnvelopeInvalid { reason: "sender" });
        }
        if envelope.nonce != first.transaction.inner().nonce() {
            return Err(CandidateError::EnvelopeInvalid { reason: "nonce" });
        }
        if envelope.rule.kind != RULE_MAX_RECIPIENT_BALANCE_DELTA {
            return Err(CandidateError::EnvelopeInvalid { reason: "rule kind" });
        }
        if envelope.rule.score_version != SCORE_VERSION_V1 {
            return Err(CandidateError::ScoreVersionUnsupported);
        }
        if envelope.fallback().is_none() {
            return Err(CandidateError::EnvelopeInvalid { reason: "fallback mode" });
        }
        if usize::from(envelope.default_index) >= siblings.len() {
            return Err(CandidateError::EnvelopeInvalid { reason: "default index" });
        }
        if envelope.disclosure > 1 {
            return Err(CandidateError::EnvelopeInvalid { reason: "disclosure" });
        }
        if envelope.max_evaluations == 0 {
            return Err(CandidateError::EnvelopeInvalid { reason: "max evaluations" });
        }
        if envelope.max_work_gas == 0 {
            return Err(CandidateError::EnvelopeInvalid { reason: "max work gas" });
        }
        Ok(())
    }

    fn check_signature(group: &CandidateGroup) -> Result<(), CandidateError> {
        let signature = group.signature();
        if signature.normalize_s().is_some() {
            return Err(CandidateError::EnvelopeSignatureInvalid);
        }
        let digest: B256 = group.group_hash();
        let signer = signature
            .recover_address_from_prehash(&digest)
            .map_err(|_| CandidateError::EnvelopeSignatureInvalid)?;
        if signer != group.sender() {
            return Err(CandidateError::EnvelopeSignerNotSender);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use rstest::rstest;

    use super::*;
    use crate::test_utils::CandidateFixture;

    const CHAIN_ID: u64 = 8453;

    fn validator() -> CandidateValidator {
        CandidateValidator { limits: CandidateLimits::default(), chain_id: CHAIN_ID }
    }

    fn window() -> Option<ExpiryWindow> {
        Some(ExpiryWindow { next_block: 10, max_blocks: 300 })
    }

    #[test]
    fn accepts_a_well_formed_group() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, envelope, signature) = fixture.submission(3, 1);
        let group = validator().validate(&raw, envelope.clone(), signature, window()).unwrap();
        assert_eq!(group.group_hash(), envelope.signing_hash(CHAIN_ID));
        assert_eq!(group.siblings().len(), 3);
        assert_eq!(group.representative().hash(), envelope.sibling_hashes[1]);
        assert_eq!(group.sender(), fixture.address());
    }

    #[rstest]
    #[case::one(1, "too_few_siblings")]
    #[case::five(5, "too_many_siblings")]
    fn rejects_sibling_counts_outside_bounds(#[case] count: usize, #[case] label: &str) {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, envelope, signature) = fixture.submission(count, 0);
        let error = validator().validate(&raw, envelope, signature, window()).unwrap_err();
        assert_eq!(error.as_label(), label);
    }

    #[test]
    fn rejects_oversized_siblings() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, envelope, signature) = fixture.submission(2, 0);
        let mut validator = validator();
        validator.limits.max_sibling_bytes = 10;
        let error = validator.validate(&raw, envelope, signature, window()).unwrap_err();
        assert_eq!(error, CandidateError::RequestTooLarge);
    }

    #[test]
    fn rejects_undecodable_and_non_eip1559_siblings() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (mut raw, envelope, signature) = fixture.submission(2, 0);
        raw[1] = Bytes::from_static(&[0x02, 0x01]);
        let error = validator().validate(&raw, envelope.clone(), signature, window()).unwrap_err();
        assert_eq!(error, CandidateError::SiblingDecodeFailed { index: 1 });

        raw[1] = fixture.legacy_sibling(envelope.nonce);
        let error = validator().validate(&raw, envelope, signature, window()).unwrap_err();
        assert_eq!(error, CandidateError::SiblingTypeUnsupported { index: 1 });
    }

    #[test]
    fn rejects_non_uniform_siblings() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let other = CandidateFixture::with_key(CHAIN_ID, 0x99);
        let base = fixture.sibling_tx(0, Address::repeat_byte(1), U256::ZERO);

        let cases = [
            (other.encode(base.clone()), "sibling_sender_mismatch"),
            (
                fixture.encode(alloy_consensus::TxEip1559 { nonce: 1, ..base.clone() }),
                "sibling_nonce_mismatch",
            ),
            (
                fixture.encode(alloy_consensus::TxEip1559 {
                    max_fee_per_gas: base.max_fee_per_gas + 1,
                    ..base.clone()
                }),
                "sibling_fee_mismatch",
            ),
            (
                fixture.encode(alloy_consensus::TxEip1559 {
                    gas_limit: base.gas_limit + 1,
                    ..base.clone()
                }),
                "sibling_gas_limit_mismatch",
            ),
            (fixture.encode(base.clone()), "duplicate_sibling"),
        ];
        for (second, label) in cases {
            let raw = vec![fixture.encode(base.clone()), second];
            let (envelope, signature) = fixture.sign_for(&raw, 0);
            let error = validator().validate(&raw, envelope, signature, window()).unwrap_err();
            assert_eq!(error.as_label(), label);
        }
    }

    #[test]
    fn rejects_gas_limit_above_cap_and_foreign_chain() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, envelope, signature) = fixture.submission(2, 0);
        let mut capped = validator();
        capped.limits.max_sibling_gas = 1;
        let error = capped.validate(&raw, envelope.clone(), signature, window()).unwrap_err();
        assert_eq!(error.as_label(), "sibling_gas_limit_too_high");

        let foreign = CandidateValidator { chain_id: 1, ..validator() };
        let error = foreign.validate(&raw, envelope, signature, window()).unwrap_err();
        assert_eq!(error.as_label(), "sibling_nonce_mismatch");
    }

    #[test]
    fn rejects_envelope_that_does_not_match_siblings() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, mut envelope, _) = fixture.submission(2, 0);
        envelope.sibling_hashes.swap(0, 1);
        let signature = fixture.sign(&envelope);
        let error = validator().validate(&raw, envelope, signature, window()).unwrap_err();
        assert_eq!(error, CandidateError::EnvelopeHashMismatch);
    }

    #[rstest]
    #[case::kind(|e: &mut CandidateEnvelope| e.rule.kind = 2, "envelope_invalid")]
    #[case::score(|e: &mut CandidateEnvelope| e.rule.score_version = 2, "score_version_unsupported")]
    #[case::fallback(|e: &mut CandidateEnvelope| e.fallback_mode = 2, "envelope_invalid")]
    #[case::default_index(|e: &mut CandidateEnvelope| e.default_index = 2, "envelope_invalid")]
    #[case::disclosure(|e: &mut CandidateEnvelope| e.disclosure = 2, "envelope_invalid")]
    #[case::evaluations(|e: &mut CandidateEnvelope| e.max_evaluations = 0, "envelope_invalid")]
    #[case::work(|e: &mut CandidateEnvelope| e.max_work_gas = 0, "envelope_invalid")]
    #[case::nonce(|e: &mut CandidateEnvelope| e.nonce += 1, "envelope_invalid")]
    #[case::past(|e: &mut CandidateEnvelope| e.expiry_block = 9, "expiry_out_of_window")]
    #[case::far(|e: &mut CandidateEnvelope| e.expiry_block = 311, "expiry_out_of_window")]
    fn rejects_invalid_envelope_fields(
        #[case] mutate: fn(&mut CandidateEnvelope),
        #[case] label: &str,
    ) {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, mut envelope, _) = fixture.submission(2, 0);
        mutate(&mut envelope);
        let signature = fixture.sign(&envelope);
        let error = validator().validate(&raw, envelope, signature, window()).unwrap_err();
        assert_eq!(error.as_label(), label);
    }

    #[test]
    fn expiry_window_is_inclusive_at_both_ends() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        for expiry in [10, 310] {
            let (raw, mut envelope, _) = fixture.submission(2, 0);
            envelope.expiry_block = expiry;
            let signature = fixture.sign(&envelope);
            validator().validate(&raw, envelope, signature, window()).unwrap();
        }
    }

    #[test]
    fn rejects_signature_from_another_key_and_malformed_signature() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let other = CandidateFixture::with_key(CHAIN_ID, 0x99);
        let (raw, envelope, _) = fixture.submission(2, 0);
        let error = validator()
            .validate(&raw, envelope.clone(), other.sign(&envelope), window())
            .unwrap_err();
        assert_eq!(error, CandidateError::EnvelopeSignerNotSender);

        let malformed = Signature::new(U256::ZERO, U256::ZERO, false);
        let error = validator().validate(&raw, envelope, malformed, window()).unwrap_err();
        assert_eq!(error, CandidateError::EnvelopeSignatureInvalid);
    }
}
