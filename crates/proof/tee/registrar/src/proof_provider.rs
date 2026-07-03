//! Platform-routed attestation proof providers.

use std::fmt;

use alloy_primitives::Address;
use base_proof_tee_attestation::{Result, TeeAttestationProof, TeeAttestationProofProvider};

use crate::AttestationKind;

/// Pair of proof providers used by the registrar for Nitro and TDX nodes.
pub struct PlatformProofProvider {
    nitro: Box<dyn TeeAttestationProofProvider>,
    tdx: Box<dyn TeeAttestationProofProvider>,
}

impl PlatformProofProvider {
    /// Creates a platform proof provider pair.
    pub fn new<N, T>(nitro: N, tdx: T) -> Self
    where
        N: TeeAttestationProofProvider + 'static,
        T: TeeAttestationProofProvider + 'static,
    {
        Self { nitro: Box::new(nitro), tdx: Box::new(tdx) }
    }

    fn provider(&self, kind: AttestationKind) -> &dyn TeeAttestationProofProvider {
        match kind {
            AttestationKind::Nitro => self.nitro.as_ref(),
            AttestationKind::Tdx => self.tdx.as_ref(),
        }
    }

    /// Generates an attestation proof for `kind`.
    pub async fn generate_proof_for_signer(
        &self,
        kind: AttestationKind,
        attestation_bytes: &[u8],
        signer_address: Address,
    ) -> Result<TeeAttestationProof> {
        self.provider(kind).generate_proof_for_signer(attestation_bytes, signer_address).await
    }

    /// Blocks recovered-proof reuse for one signer on the selected platform.
    pub fn block_recovery_for_signer(&self, kind: AttestationKind, signer: Address) {
        self.provider(kind).block_recovery_for_signer(signer);
    }
}

impl fmt::Debug for PlatformProofProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PlatformProofProvider")
    }
}
