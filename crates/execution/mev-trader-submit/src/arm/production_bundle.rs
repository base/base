//! Canonical owner-signed production proof bundle decoding and per-candidate verification.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{B256, Signature, keccak256};
use base_mev_trader::{CampaignId, OWNER_ATTEST_ADDRESS, StoreIdentity};

use super::{
    CodeHashProvider, DeploymentEvidence, DeploymentPayload, G7Attestation, G7Payload,
    INSTALL_BUNDLE_DOMAIN, LiveRunAttestation, LiveRunPayload, ProductionCampaignBundleFailure,
    ProductionCandidateError, ProofVerificationError, SETTLED_LOSS_SCHEMA_VERSION,
    settled_loss::{read_install_bundle_bytes, verify_signature_shape},
};

const PRODUCTION_BUNDLE_BYTES: usize = 584;

/// Fresh consuming checked proofs for exactly one candidate authorization.
#[derive(Debug)]
pub struct VerifiedProductionProofs {
    /// Fresh G7 closure proof.
    pub g7: G7Attestation,
    /// Fresh live-window proof.
    pub live: LiveRunAttestation,
    /// Fresh deployment/provider proof.
    pub deployment: DeploymentEvidence,
}

/// Raw canonical owner-signed proof bundle retained by the production worker.
#[derive(Debug)]
pub struct ProductionProofBundle {
    canonical: [u8; PRODUCTION_BUNDLE_BYTES],
    generation: u64,
    g7: G7Payload,
    g7_signature: [u8; 65],
    live: LiveRunPayload,
    live_signature: [u8; 65],
    deployment: DeploymentPayload,
    deployment_signature: [u8; 65],
    content_hash: B256,
    outer_signature: [u8; 65],
}

impl ProductionProofBundle {
    /// Loads and strictly decodes the compile-pinned canonical bundle.
    pub fn load() -> Result<Self, ProductionCampaignBundleFailure> {
        let bytes = read_install_bundle_bytes().map_err(|reason| match reason {
            super::SettledLossUnavailableReason::Missing => {
                ProductionCampaignBundleFailure::Missing
            }
            super::SettledLossUnavailableReason::Malformed => {
                ProductionCampaignBundleFailure::Decode
            }
            super::SettledLossUnavailableReason::AuthenticationFailed => {
                ProductionCampaignBundleFailure::Signature
            }
            super::SettledLossUnavailableReason::ManifestMismatch
            | super::SettledLossUnavailableReason::CanonicalMismatch(_) => {
                ProductionCampaignBundleFailure::Identity
            }
            super::SettledLossUnavailableReason::Incomplete
            | super::SettledLossUnavailableReason::Unresolved(_)
            | super::SettledLossUnavailableReason::Stale
            | super::SettledLossUnavailableReason::FinalityUnavailable
            | super::SettledLossUnavailableReason::Rollback
            | super::SettledLossUnavailableReason::Io => ProductionCampaignBundleFailure::Io,
        })?;
        Self::decode(&bytes)
    }

    /// Decodes exact fixed-order bytes and verifies the outer generation binding.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProductionCampaignBundleFailure> {
        let canonical: [u8; PRODUCTION_BUNDLE_BYTES] =
            bytes.try_into().map_err(|_| ProductionCampaignBundleFailure::Bounds)?;
        let mut cursor = 0;
        let domain = take::<30>(&canonical, &mut cursor);
        if &domain != INSTALL_BUNDLE_DOMAIN {
            return Err(ProductionCampaignBundleFailure::Identity);
        }
        let schema = u16::from_be_bytes(take::<2>(&canonical, &mut cursor));
        if schema != SETTLED_LOSS_SCHEMA_VERSION {
            return Err(ProductionCampaignBundleFailure::Identity);
        }
        let generation = u64::from_be_bytes(take::<8>(&canonical, &mut cursor));
        if generation == 0 {
            return Err(ProductionCampaignBundleFailure::Identity);
        }
        let g7 = G7Payload {
            campaign_id: CampaignId::new(take::<32>(&canonical, &mut cursor)),
            g7_closure_epoch: u64::from_be_bytes(take::<8>(&canonical, &mut cursor)),
            expiry_unix: u64::from_be_bytes(take::<8>(&canonical, &mut cursor)),
        };
        let g7_signature = take::<65>(&canonical, &mut cursor);
        let live = LiveRunPayload {
            campaign_id: CampaignId::new(take::<32>(&canonical, &mut cursor)),
            window_start: u64::from_be_bytes(take::<8>(&canonical, &mut cursor)),
            expiry_unix: u64::from_be_bytes(take::<8>(&canonical, &mut cursor)),
        };
        let live_signature = take::<65>(&canonical, &mut cursor);
        let deployment = DeploymentPayload {
            chain_id: u64::from_be_bytes(take::<8>(&canonical, &mut cursor)),
            executor: alloy_primitives::Address::from(take::<20>(&canonical, &mut cursor)),
            code_hash: B256::from(take::<32>(&canonical, &mut cursor)),
            binary_digest: B256::from(take::<32>(&canonical, &mut cursor)),
            deployment_digest: B256::from(take::<32>(&canonical, &mut cursor)),
            r9_store_identity: StoreIdentity::new(take::<32>(&canonical, &mut cursor)),
        };
        let deployment_signature = take::<65>(&canonical, &mut cursor);
        let content_hash = B256::from(take::<32>(&canonical, &mut cursor));
        let outer_signature = take::<65>(&canonical, &mut cursor);
        debug_assert_eq!(cursor, PRODUCTION_BUNDLE_BYTES);
        let bundle = Self {
            canonical,
            generation,
            g7,
            g7_signature,
            live,
            live_signature,
            deployment,
            deployment_signature,
            content_hash,
            outer_signature,
        };
        bundle.verify_outer()?;
        Ok(bundle)
    }

    /// Verifies fresh consuming inner proofs exactly once each for one candidate.
    pub fn verify_candidate(
        &self,
        provider: &dyn CodeHashProvider,
    ) -> Result<VerifiedProductionProofs, ProductionCandidateError> {
        self.verify_outer().map_err(|_| {
            ProductionCandidateError::G7(ProofVerificationError::CanonicalSignature)
        })?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProductionCandidateError::Live(ProofVerificationError::WindowNotStarted))?
            .as_secs();
        let g7 = G7Attestation::verify_checked(&self.g7, &self.g7_signature, now)
            .map_err(ProductionCandidateError::G7)?;
        let live = LiveRunAttestation::verify_checked(&self.live, &self.live_signature, now)
            .map_err(ProductionCandidateError::Live)?;
        let deployment = DeploymentEvidence::verify_checked(
            &self.deployment,
            &self.deployment_signature,
            provider,
        )
        .map_err(ProductionCandidateError::Deployment)?;
        if g7.campaign_id() != live.campaign_id() {
            return Err(ProductionCandidateError::CampaignMismatch);
        }
        Ok(VerifiedProductionProofs { g7, live, deployment })
    }

    /// Returns the authenticated campaign shared by the proof pair.
    pub const fn campaign_id(&self) -> CampaignId {
        self.g7.campaign_id
    }

    /// Verifies deployment/provider evidence for startup identity and claim-store binding.
    pub fn verify_deployment(
        &self,
        provider: &dyn CodeHashProvider,
    ) -> Result<DeploymentEvidence, ProductionCandidateError> {
        self.verify_outer().map_err(|_| {
            ProductionCandidateError::Deployment(ProofVerificationError::CanonicalSignature)
        })?;
        DeploymentEvidence::verify_checked(&self.deployment, &self.deployment_signature, provider)
            .map_err(ProductionCandidateError::Deployment)
    }

    fn verify_outer(&self) -> Result<(), ProductionCampaignBundleFailure> {
        let g7_start = 40;
        let live_start = g7_start + 113;
        let deployment_start = live_start + 113;
        let g7_hash = keccak256(&self.canonical[g7_start..live_start]);
        let live_hash = keccak256(&self.canonical[live_start..deployment_start]);
        let deployment_hash = keccak256(&self.canonical[deployment_start..deployment_start + 221]);
        let mut content = Vec::with_capacity(30 + 2 + 8 + 32 * 3);
        content.extend_from_slice(INSTALL_BUNDLE_DOMAIN);
        content.extend_from_slice(&SETTLED_LOSS_SCHEMA_VERSION.to_be_bytes());
        content.extend_from_slice(&self.generation.to_be_bytes());
        content.extend_from_slice(g7_hash.as_slice());
        content.extend_from_slice(live_hash.as_slice());
        content.extend_from_slice(deployment_hash.as_slice());
        if keccak256(&content) != self.content_hash {
            return Err(ProductionCampaignBundleFailure::MixedGeneration);
        }
        verify_signature_shape(&self.outer_signature)
            .map_err(|_| ProductionCampaignBundleFailure::Signature)?;
        let owner = OWNER_ATTEST_ADDRESS.ok_or(ProductionCampaignBundleFailure::Identity)?;
        let signature = Signature::from_raw_array(&self.outer_signature)
            .map_err(|_| ProductionCampaignBundleFailure::Signature)?;
        let mut outer = Vec::with_capacity(62);
        outer.extend_from_slice(INSTALL_BUNDLE_DOMAIN);
        outer.extend_from_slice(self.content_hash.as_slice());
        let recovered = signature
            .recover_address_from_msg(&outer)
            .map_err(|_| ProductionCampaignBundleFailure::Signature)?;
        if recovered != owner {
            return Err(ProductionCampaignBundleFailure::Identity);
        }
        Ok(())
    }
}

fn take<const N: usize>(bytes: &[u8; PRODUCTION_BUNDLE_BYTES], cursor: &mut usize) -> [u8; N] {
    let end = *cursor + N;
    let value = bytes[*cursor..end].try_into().expect("fixed bundle bounds");
    *cursor = end;
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimally_shaped() -> [u8; PRODUCTION_BUNDLE_BYTES] {
        let mut bytes = [0_u8; PRODUCTION_BUNDLE_BYTES];
        bytes[..30].copy_from_slice(INSTALL_BUNDLE_DOMAIN);
        bytes[30..32].copy_from_slice(&SETTLED_LOSS_SCHEMA_VERSION.to_be_bytes());
        bytes[32..40].copy_from_slice(&1_u64.to_be_bytes());
        bytes
    }

    #[test]
    fn canonical_bundle_rejects_trailing_bytes_and_zero_generation() {
        let bytes = minimally_shaped();
        assert_eq!(
            ProductionProofBundle::decode(&[bytes.as_slice(), &[0]].concat()).unwrap_err(),
            ProductionCampaignBundleFailure::Bounds
        );
        let mut zero_generation = bytes;
        zero_generation[32..40].fill(0);
        assert_eq!(
            ProductionProofBundle::decode(&zero_generation).unwrap_err(),
            ProductionCampaignBundleFailure::Identity
        );
    }

    #[test]
    fn canonical_bundle_rejects_mixed_generation_before_outer_signature() {
        assert_eq!(
            ProductionProofBundle::decode(&minimally_shaped()).unwrap_err(),
            ProductionCampaignBundleFailure::MixedGeneration
        );
    }
}
