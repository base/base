//! Platform-routed attestation proof providers.

use alloy_primitives::Address;
use base_proof_tee_attestation::{Result, TeeAttestationProof, TeeAttestationProofProvider};

use crate::AttestationKind;

/// Pair of proof providers used by the registrar for Nitro and TDX nodes.
#[derive(Debug, Clone)]
pub struct PlatformProofProvider<N, T> {
    nitro: N,
    tdx: T,
}

impl<N, T> PlatformProofProvider<N, T> {
    /// Creates a platform proof provider pair.
    pub const fn new(nitro: N, tdx: T) -> Self {
        Self { nitro, tdx }
    }
}

impl<N, T> PlatformProofProvider<N, T>
where
    N: TeeAttestationProofProvider,
    T: TeeAttestationProofProvider,
{
    /// Generates an attestation proof for `kind`.
    pub async fn generate_proof_for_signer(
        &self,
        kind: AttestationKind,
        attestation_bytes: &[u8],
        signer_address: Address,
    ) -> Result<TeeAttestationProof> {
        match kind {
            AttestationKind::Nitro => {
                self.nitro.generate_proof_for_signer(attestation_bytes, signer_address).await
            }
            AttestationKind::Tdx => {
                self.tdx.generate_proof_for_signer(attestation_bytes, signer_address).await
            }
        }
    }

    /// Blocks recovered-proof reuse for one signer on the selected platform.
    pub fn block_recovery_for_signer(&self, kind: AttestationKind, signer: Address) {
        match kind {
            AttestationKind::Nitro => self.nitro.block_recovery_for_signer(signer),
            AttestationKind::Tdx => self.tdx.block_recovery_for_signer(signer),
        }
    }
}
