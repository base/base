//! The R9 victim-claim façade. Maps the real [`VictimClaimStore::try_claim`]
//! result into the arm error model: a successful `Claimed` yields the
//! (non-`Clone`) proof; `AlreadyClaimed` is a normal, NON-latching refusal; any
//! store error is a fail-stop latch (durable kill + process poison).

use std::sync::Arc;

use alloy_primitives::B256;
use base_mev_trader::{
    CampaignId, ClaimResult, ClaimStoreError, KillReason, VictimClaim, VictimClaimStore,
};

use super::{ArmError, ArmedFailSink, ProductionLatchOutcome};

/// Stable bounded class for a production claim-store failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionClaimFailure {
    /// Storage I/O failed.
    Io,
    /// Existing state was structurally corrupt.
    Corruption,
    /// Commit durability is unknown.
    CommitUnknown,
    /// Another singleton writer owns the store.
    NotSingleton,
    /// The store identity did not match deployment evidence.
    IdentityMismatch,
}

/// Exact source and mandatory latch result for a failed production claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductionClaimError {
    source: ProductionClaimFailure,
    latch: ProductionLatchOutcome,
}

impl ProductionClaimError {
    /// Returns the stable claim failure class.
    pub const fn source(&self) -> ProductionClaimFailure {
        self.source
    }

    /// Returns the mandatory fail-stop latch outcome.
    pub const fn latch(&self) -> ProductionLatchOutcome {
        self.latch
    }
}

/// Detailed production claim result preserving ordinary duplicate refusal separately from faults.
#[derive(Debug)]
pub enum ProductionClaimResult {
    /// The claim was durably issued.
    Claimed(VictimClaim),
    /// The victim had already been claimed; no latch was attempted.
    AlreadyClaimed,
}

/// Attempt an at-most-once victim claim through the shared fail-stop sink.
///
/// * `Claimed(proof)` -> `Ok(proof)`.
/// * `AlreadyClaimed` -> `Err(ArmError::AlreadyClaimed)` (no latch — a legitimate
///   global refusal, not a fault).
/// * any `ClaimStoreError` -> latch the durable fail-stop and return the mapped
///   error (the outcome is unknown / the store is untrustworthy, so halt).
pub fn try_claim_arm(
    store: &VictimClaimStore,
    chain_id: u64,
    victim_tx_hash: B256,
    campaign_id: CampaignId,
    sink: &Arc<ArmedFailSink>,
) -> Result<VictimClaim, ArmError> {
    sink.check()?;
    match store.try_claim(chain_id, victim_tx_hash, campaign_id) {
        Ok(ClaimResult::Claimed(proof)) => Ok(proof),
        Ok(ClaimResult::AlreadyClaimed) => Err(ArmError::AlreadyClaimed),
        Err(_) => Err(sink.latch(KillReason::KeyOrSignatureFailure)),
    }
}

/// Issues the sole production claim while preserving source and latch outcomes.
pub fn try_claim_detailed(
    store: &VictimClaimStore,
    chain_id: u64,
    victim_tx_hash: B256,
    campaign_id: CampaignId,
    sink: &Arc<ArmedFailSink>,
) -> Result<ProductionClaimResult, ProductionClaimError> {
    match store.try_claim(chain_id, victim_tx_hash, campaign_id) {
        Ok(ClaimResult::Claimed(proof)) => Ok(ProductionClaimResult::Claimed(proof)),
        Ok(ClaimResult::AlreadyClaimed) => Ok(ProductionClaimResult::AlreadyClaimed),
        Err(error) => {
            let source = match error {
                ClaimStoreError::Io(_) => ProductionClaimFailure::Io,
                ClaimStoreError::Corruption(_) => ProductionClaimFailure::Corruption,
                ClaimStoreError::CommitFailed(_) => ProductionClaimFailure::CommitUnknown,
                ClaimStoreError::NotSingletonWriter => ProductionClaimFailure::NotSingleton,
                ClaimStoreError::StoreIdentityMismatch => ProductionClaimFailure::IdentityMismatch,
            };
            Err(ProductionClaimError { source, latch: sink.latch_production() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arm::testkit as tk;
    use alloy_primitives::B256;

    fn campaign() -> CampaignId {
        CampaignId::new([0x0Au8; 32])
    }

    #[test]
    fn claimed_returns_proof_no_poison() {
        let dir = tk::TempDir::new("claim-ok");
        let config = base_mev_trader::VictimClaimConfig { db_path: dir.path.join("claims.redb") };
        let store = VictimClaimStore::bootstrap(&config).expect("bootstrap");
        let sink = tk::sink(&dir.path);
        let victim = B256::repeat_byte(0xC1);
        let claim = try_claim_arm(&store, 8453, victim, campaign(), &sink).expect("claim");
        assert_eq!(claim.victim_tx_hash(), victim);
        assert!(!sink.is_poisoned());
    }

    #[test]
    fn already_claimed_is_non_latching_refusal() {
        let dir = tk::TempDir::new("claim-dup");
        let config = base_mev_trader::VictimClaimConfig { db_path: dir.path.join("claims.redb") };
        let store = VictimClaimStore::bootstrap(&config).expect("bootstrap");
        let sink = tk::sink(&dir.path);
        let victim = B256::repeat_byte(0xC2);
        // First claim succeeds; second is AlreadyClaimed (NOT a latch).
        try_claim_arm(&store, 8453, victim, campaign(), &sink).expect("first");
        let err = try_claim_arm(&store, 8453, victim, campaign(), &sink).unwrap_err();
        assert!(matches!(err, ArmError::AlreadyClaimed));
        assert!(!sink.is_poisoned(), "AlreadyClaimed must not poison");
    }

    #[test]
    fn poisoned_sink_refuses_before_store() {
        let dir = tk::TempDir::new("claim-poison");
        let config = base_mev_trader::VictimClaimConfig { db_path: dir.path.join("claims.redb") };
        let store = VictimClaimStore::bootstrap(&config).expect("bootstrap");
        let sink = tk::sink(&dir.path);
        sink.latch(KillReason::KeyOrSignatureFailure);
        let err =
            try_claim_arm(&store, 8453, B256::repeat_byte(0xC3), campaign(), &sink).unwrap_err();
        assert!(matches!(err, ArmError::Poisoned));
    }
}
