//! The R9 victim-claim façade. Maps the real [`VictimClaimStore::try_claim`]
//! result into the arm error model: a successful `Claimed` yields the
//! (non-`Clone`) proof; `AlreadyClaimed` is a normal, NON-latching refusal; any
//! store error is a fail-stop latch (durable kill + process poison).

use std::sync::Arc;

use alloy_primitives::B256;
use base_mev_trader::{CampaignId, ClaimResult, KillReason, VictimClaim, VictimClaimStore};

use super::{ArmError, ArmedFailSink};

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
        let err = try_claim_arm(&store, 8453, B256::repeat_byte(0xC3), campaign(), &sink).unwrap_err();
        assert!(matches!(err, ArmError::Poisoned));
    }
}
