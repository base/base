//! Platform-routed attestation proof providers.

use std::future::Future;

use alloy_primitives::Address;
use base_proof_tee_attestation::{BoxError, TeeAttestationProof, TeeAttestationProofProvider};
use tokio_util::sync::CancellationToken;

use crate::AttestationKind;

/// Registrar proof provider that can generate platform-specific attestation proofs.
pub trait RegistrarProofProvider: Send + Sync {
    /// Generates an attestation proof for `kind`.
    fn generate_proof_for_signer<'a>(
        &'a self,
        kind: AttestationKind,
        attestation_bytes: &'a [u8],
        signer_address: Address,
        cancel: &'a CancellationToken,
    ) -> impl Future<Output = Result<TeeAttestationProof, BoxError>> + Send + 'a;

    /// Blocks recovered-proof reuse for one signer on the selected platform.
    fn block_recovery_for_signer(&self, kind: AttestationKind, signer: Address);
}

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

impl<N, T> RegistrarProofProvider for PlatformProofProvider<N, T>
where
    N: TeeAttestationProofProvider,
    T: TeeAttestationProofProvider,
{
    async fn generate_proof_for_signer(
        &self,
        kind: AttestationKind,
        attestation_bytes: &[u8],
        signer_address: Address,
        _cancel: &CancellationToken,
    ) -> Result<TeeAttestationProof, BoxError> {
        match kind {
            AttestationKind::Nitro => {
                self.nitro.generate_proof_for_signer(attestation_bytes, signer_address).await
            }
            AttestationKind::Tdx => {
                self.tdx.generate_proof_for_signer(attestation_bytes, signer_address).await
            }
        }
    }

    fn block_recovery_for_signer(&self, kind: AttestationKind, signer: Address) {
        match kind {
            AttestationKind::Nitro => self.nitro.block_recovery_for_signer(signer),
            AttestationKind::Tdx => self.tdx.block_recovery_for_signer(signer),
        }
    }
}
