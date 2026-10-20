//! Stateless admission checks for a candidate submission (design §2.3 steps 3–5, §4.2).

use alloy_consensus::{
    Transaction,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{B256, Bytes, Signature, keccak256};
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
    /// Maximum transactions per swap, counting the default transaction and its alternatives;
    /// at most [`MAX_SIBLINGS_CEILING`].
    pub max_siblings: usize,
    /// Maximum encoded size of one transaction, including a default transaction submitted
    /// alone.
    pub max_sibling_bytes: usize,
    /// Maximum encoded size of all siblings in a request.
    pub max_request_bytes: usize,
    /// Maximum gas limit of any swap transaction: the shared limit of a group's siblings, and
    /// the limit of a default transaction submitted alone.
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

/// Validates untrusted candidate submissions into [`CandidateGroup`]s.
#[derive(Debug, Clone, Copy)]
pub struct CandidateValidator {
    /// Bounds applied to every submission.
    pub limits: CandidateLimits,
    /// The only chain ID siblings may carry.
    pub chain_id: u64,
}

impl CandidateValidator {
    /// Runs bounds, decode, uniformity, envelope and signature checks, in that order.
    ///
    /// `expiry_block` is the node-assigned last block of the group's lifetime, usually
    /// [`crate::BlockWindow::last_block`] of the configured lifetime from the next block.
    pub fn validate(
        &self,
        raw_siblings: &[Bytes],
        envelope: CandidateEnvelope,
        signature: Signature,
        expiry_block: u64,
    ) -> Result<CandidateGroup, CandidateError> {
        self.check_bounds(raw_siblings)?;
        let siblings = Self::decode(raw_siblings)?;
        self.check_uniformity(&siblings)?;
        Self::check_envelope(&envelope, &siblings)?;
        let group = CandidateGroup::new(envelope, signature, self.chain_id, siblings, expiry_block);
        Self::check_signature(&group)?;
        Ok(group)
    }

    /// Validates a default transaction with alternatives under a signed selection envelope.
    ///
    /// The set (default plus alternatives) gets the bounds of [`Self::check_bounds`] first; then
    /// [`Self::arrange`] puts it in envelope order and [`Self::validate`] runs.
    pub fn validate_selection(
        &self,
        default: &Bytes,
        alternatives: &[Bytes],
        envelope: CandidateEnvelope,
        signature: Signature,
        expiry_block: u64,
    ) -> Result<CandidateGroup, CandidateError> {
        let set: Vec<Bytes> = core::iter::once(default).chain(alternatives).cloned().collect();
        self.check_bounds(&set)?;
        let ordered = Self::arrange(default, alternatives, &envelope)?;
        self.validate(&ordered, envelope, signature, expiry_block)
    }

    /// Orders a default transaction and its alternatives as the envelope lists them.
    ///
    /// The default transaction must be the envelope's representative: `siblingHashes[defaultIndex]`
    /// under `DEFAULT_SIBLING`, `siblingHashes[0]` under `DROP`. The alternatives fill the other
    /// positions, in envelope order.
    pub fn arrange(
        default: &Bytes,
        alternatives: &[Bytes],
        envelope: &CandidateEnvelope,
    ) -> Result<Vec<Bytes>, CandidateError> {
        if envelope.sibling_hashes.len() != alternatives.len() + 1 {
            return Err(CandidateError::EnvelopeHashMismatch);
        }
        let index = envelope.representative_index();
        if index > alternatives.len() {
            return Err(CandidateError::EnvelopeInvalid { reason: "default index" });
        }
        if envelope.sibling_hashes[index] != keccak256(default) {
            return Err(CandidateError::DefaultMismatch);
        }
        let mut ordered = alternatives.to_vec();
        ordered.insert(index, default.clone());
        Ok(ordered)
    }

    /// Checks a default transaction submitted without alternatives.
    ///
    /// It gets the per-member size, type and gas bounds of a set, including
    /// [`CandidateLimits::max_sibling_gas`], so a request is admissible the same way whether or
    /// not selection applies to it. The pool checks the rest.
    pub fn validate_default(&self, raw: &Bytes) -> Result<CandidateSibling, CandidateError> {
        if raw.len() > self.limits.max_sibling_bytes {
            return Err(CandidateError::RequestTooLarge);
        }
        let sibling = Self::decode(core::slice::from_ref(raw))?
            .pop()
            .ok_or(CandidateError::SiblingDecodeFailed { index: 0 })?;
        if sibling.transaction.inner().gas_limit() > self.limits.max_sibling_gas {
            return Err(CandidateError::SiblingGasLimitTooHigh {
                max: self.limits.max_sibling_gas,
            });
        }
        Ok(sibling)
    }

    /// Checks sibling count and request byte bounds before any decoding.
    pub fn check_bounds(&self, raw_siblings: &[Bytes]) -> Result<(), CandidateError> {
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

    /// Decodes and recovers each sibling, rejecting non-EIP-1559 encodings.
    pub fn decode(raw_siblings: &[Bytes]) -> Result<Vec<CandidateSibling>, CandidateError> {
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

    /// Checks that siblings share sender, nonce, fees and gas limit, are distinct, and stay
    /// within the sibling gas cap (design §4.2).
    pub fn check_uniformity(&self, siblings: &[CandidateSibling]) -> Result<(), CandidateError> {
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

    /// Checks the envelope against the siblings and the configured limits.
    pub fn check_envelope(
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

    /// Checks that the envelope signer is the siblings' sender.
    pub fn check_signature(group: &CandidateGroup) -> Result<(), CandidateError> {
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

    /// A node-assigned lifetime end; validation does not interpret it.
    const EXPIRY: u64 = 100;

    #[test]
    fn accepts_a_well_formed_group() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, envelope, signature) = fixture.submission(3, 1);
        let group = validator().validate(&raw, envelope.clone(), signature, EXPIRY).unwrap();
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
        let error = validator().validate(&raw, envelope, signature, EXPIRY).unwrap_err();
        assert_eq!(error.as_label(), label);
    }

    #[test]
    fn rejects_oversized_siblings() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, envelope, signature) = fixture.submission(2, 0);
        let mut validator = validator();
        validator.limits.max_sibling_bytes = 10;
        let error = validator.validate(&raw, envelope, signature, EXPIRY).unwrap_err();
        assert_eq!(error, CandidateError::RequestTooLarge);
    }

    #[test]
    fn rejects_undecodable_and_non_eip1559_siblings() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (mut raw, envelope, signature) = fixture.submission(2, 0);
        raw[1] = Bytes::from_static(&[0x02, 0x01]);
        let error = validator().validate(&raw, envelope.clone(), signature, EXPIRY).unwrap_err();
        assert_eq!(error, CandidateError::SiblingDecodeFailed { index: 1 });

        raw[1] = fixture.legacy_sibling(envelope.nonce);
        let error = validator().validate(&raw, envelope, signature, EXPIRY).unwrap_err();
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
            let error = validator().validate(&raw, envelope, signature, EXPIRY).unwrap_err();
            assert_eq!(error.as_label(), label);
        }
    }

    #[test]
    fn rejects_gas_limit_above_cap_and_foreign_chain() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, envelope, signature) = fixture.submission(2, 0);
        let mut capped = validator();
        capped.limits.max_sibling_gas = 1;
        let error = capped.validate(&raw, envelope.clone(), signature, EXPIRY).unwrap_err();
        assert_eq!(error.as_label(), "sibling_gas_limit_too_high");

        let foreign = CandidateValidator { chain_id: 1, ..validator() };
        let error = foreign.validate(&raw, envelope, signature, EXPIRY).unwrap_err();
        assert_eq!(error.as_label(), "sibling_nonce_mismatch");
    }

    #[test]
    fn rejects_envelope_that_does_not_match_siblings() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, mut envelope, _) = fixture.submission(2, 0);
        envelope.sibling_hashes.swap(0, 1);
        let signature = fixture.sign(&envelope);
        let error = validator().validate(&raw, envelope, signature, EXPIRY).unwrap_err();
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
    fn rejects_invalid_envelope_fields(
        #[case] mutate: fn(&mut CandidateEnvelope),
        #[case] label: &str,
    ) {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, mut envelope, _) = fixture.submission(2, 0);
        mutate(&mut envelope);
        let signature = fixture.sign(&envelope);
        let error = validator().validate(&raw, envelope, signature, EXPIRY).unwrap_err();
        assert_eq!(error.as_label(), label);
    }

    #[rstest]
    #[case::default_sibling_first(3, 0, 1)]
    #[case::default_sibling_last(3, 2, 1)]
    #[case::drop_uses_the_first(3, 2, 0)]
    fn selection_puts_the_default_at_the_representative(
        #[case] count: usize,
        #[case] default_index: u8,
        #[case] fallback_mode: u8,
    ) {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, mut envelope, _) = fixture.submission(count, default_index);
        envelope.fallback_mode = fallback_mode;
        let signature = fixture.sign(&envelope);
        let (default, alternatives) = CandidateFixture::split_default(&raw, &envelope);

        let group = validator()
            .validate_selection(&default, &alternatives, envelope.clone(), signature, EXPIRY)
            .unwrap();
        assert_eq!(group.representative().encoded, default);
        assert_eq!(group.siblings().len(), count);
        assert_eq!(group.group_hash(), envelope.signing_hash(CHAIN_ID));
    }

    #[test]
    fn selection_rejects_a_default_that_is_not_the_representative() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, envelope, signature) = fixture.submission(3, 1);
        let error = validator()
            .validate_selection(&raw[0], &raw[1..], envelope.clone(), signature, EXPIRY)
            .unwrap_err();
        assert_eq!(error.as_label(), "default_mismatch");

        let (default, alternatives) = CandidateFixture::split_default(&raw, &envelope);
        let error = validator()
            .validate_selection(&default, &alternatives[..1], envelope, signature, EXPIRY)
            .unwrap_err();
        assert_eq!(error.as_label(), "envelope_hash_mismatch");
    }

    #[test]
    fn selection_bounds_count_the_default() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, envelope, signature) = fixture.submission(5, 0);
        let (default, alternatives) = CandidateFixture::split_default(&raw, &envelope);
        let error = validator()
            .validate_selection(&default, &alternatives, envelope, signature, EXPIRY)
            .unwrap_err();
        assert_eq!(error.as_label(), "too_many_siblings");
    }

    #[test]
    fn default_without_alternatives_gets_member_bounds() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, _, _) = fixture.submission(2, 0);
        let sibling = validator().validate_default(&raw[0]).unwrap();
        assert_eq!(sibling.encoded, raw[0]);
        assert_eq!(sibling.transaction.signer(), fixture.address());

        let mut capped = validator();
        capped.limits.max_sibling_gas = 1;
        assert_eq!(
            capped.validate_default(&raw[0]).unwrap_err().as_label(),
            "sibling_gas_limit_too_high"
        );
        capped.limits.max_sibling_bytes = 10;
        assert_eq!(capped.validate_default(&raw[0]).unwrap_err(), CandidateError::RequestTooLarge);
        assert_eq!(
            validator().validate_default(&fixture.legacy_sibling(0)).unwrap_err(),
            CandidateError::SiblingTypeUnsupported { index: 0 }
        );
    }

    #[test]
    fn rejects_signature_from_another_key_and_malformed_signature() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let other = CandidateFixture::with_key(CHAIN_ID, 0x99);
        let (raw, envelope, _) = fixture.submission(2, 0);
        let error = validator()
            .validate(&raw, envelope.clone(), other.sign(&envelope), EXPIRY)
            .unwrap_err();
        assert_eq!(error, CandidateError::EnvelopeSignerNotSender);

        let malformed = Signature::new(U256::ZERO, U256::ZERO, false);
        let error = validator().validate(&raw, envelope, malformed, EXPIRY).unwrap_err();
        assert_eq!(error, CandidateError::EnvelopeSignatureInvalid);
    }

    #[test]
    fn server_lifetime_sets_expiry_without_changing_the_group_hash() {
        let fixture = CandidateFixture::new(CHAIN_ID);
        let (raw, envelope, signature) = fixture.submission(2, 0);
        let validator =
            CandidateValidator { limits: CandidateLimits::default(), chain_id: CHAIN_ID };
        let early = validator.validate(&raw, envelope.clone(), signature, 120).unwrap();
        let late = validator.validate(&raw, envelope, signature, 150).unwrap();
        assert_eq!(early.group_hash(), late.group_hash());
        assert_eq!((early.expiry_block(), late.expiry_block()), (120, 150));
        assert_eq!(late.descriptor().expiry_block, 150);
    }
}
