//! Platform-routed attestation proof providers.

use std::{
    fmt,
    io::{Error, ErrorKind},
    time::UNIX_EPOCH,
};

use alloy_primitives::Address;
use base_proof_tee_attestation::{
    Result, TeeAttestationKind, TeeAttestationProof, TeeAttestationProofProvider,
};
use base_proof_tee_tdx_collateral::{TdxAttestationConfig, TdxAttestationHydrator};
use base_proof_tee_tdx_verifier::{TdxSignerAttestation, TdxVerifier, TdxVerifierInput};

/// Hydrates a prover's compact TDX attestation into the verifier input required
/// by a TDX proof provider.
#[derive(Debug)]
pub struct HydratedTdxProofProvider<P> {
    provider: P,
}

impl<P> HydratedTdxProofProvider<P> {
    /// Creates a TDX proof provider that fetches the required Intel collateral.
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    async fn verifier_input(
        attestation_bytes: &[u8],
        signer_address: Address,
    ) -> Result<TdxVerifierInput> {
        let attestation = TdxSignerAttestation::decode(attestation_bytes)
            .map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
        let public_key_hash = TdxVerifier::validate_public_key(&attestation.signer_public_key)
            .map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
        let actual_signer = Address::from_slice(&public_key_hash.as_slice()[12..]);
        if actual_signer != signer_address {
            return Err(Box::new(Error::new(
                ErrorKind::InvalidData,
                format!("TDX signer mismatch: expected {signer_address}, got {actual_signer}"),
            )));
        }

        let config = TdxAttestationConfig::intel_pcs();
        let hydrator = TdxAttestationHydrator::new(config.clone())
            .map_err(|error| Error::other(error.to_string()))?;
        let collateral = hydrator
            .fetch_collateral(&attestation.quote)
            .await
            .map_err(|error| Error::other(error.to_string()))?;
        let verification_time =
            UNIX_EPOCH.elapsed().map_err(|error| Error::other(error.to_string()))?.as_secs();

        Ok(TdxVerifierInput {
            quote: attestation.quote,
            pck_certificate_chain: collateral.pck_certificate_chain,
            collateral: collateral.collateral,
            revocation: collateral.revocation,
            trusted_root_ca_hash: config.trusted_root_ca_hash,
            expected_public_key: attestation.signer_public_key,
            attestation_nonce: attestation.attestation_nonce,
            workload_digest: attestation.workload_digest,
            quote_timestamp_millis: attestation.quote_timestamp_millis,
            chain_id: attestation.chain_id,
            registry_address: attestation.registry_address,
            verification_time,
            max_quote_age_seconds: config.max_quote_age.as_secs(),
            allowed_tcb_statuses: config.allowed_tcb_statuses,
        })
    }
}

#[async_trait::async_trait]
impl<P> TeeAttestationProofProvider for HydratedTdxProofProvider<P>
where
    P: TeeAttestationProofProvider,
{
    async fn generate_proof_for_signer(
        &self,
        attestation_bytes: &[u8],
        signer_address: Address,
    ) -> Result<TeeAttestationProof> {
        let input = Self::verifier_input(attestation_bytes, signer_address).await?;
        self.provider.generate_proof_for_signer(&input.encode(), signer_address).await
    }

    fn block_recovery_for_signer(&self, signer: Address) {
        self.provider.block_recovery_for_signer(signer);
    }
}

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

    fn provider(&self, kind: TeeAttestationKind) -> &dyn TeeAttestationProofProvider {
        match kind {
            TeeAttestationKind::Nitro => self.nitro.as_ref(),
            TeeAttestationKind::Tdx => self.tdx.as_ref(),
        }
    }

    /// Generates an attestation proof for `kind`.
    pub async fn generate_proof_for_signer(
        &self,
        kind: TeeAttestationKind,
        attestation_bytes: &[u8],
        signer_address: Address,
    ) -> Result<TeeAttestationProof> {
        self.provider(kind).generate_proof_for_signer(attestation_bytes, signer_address).await
    }

    /// Blocks recovered-proof reuse for one signer on the selected platform.
    pub fn block_recovery_for_signer(&self, kind: TeeAttestationKind, signer: Address) {
        self.provider(kind).block_recovery_for_signer(signer);
    }
}

impl fmt::Debug for PlatformProofProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PlatformProofProvider")
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use async_trait::async_trait;
    use base_proof_tee_attestation::TeeAttestationProof;

    use super::*;

    #[derive(Debug)]
    struct NoopProvider;

    #[async_trait]
    impl TeeAttestationProofProvider for NoopProvider {
        async fn generate_proof_for_signer(
            &self,
            _attestation_bytes: &[u8],
            _signer_address: Address,
        ) -> Result<TeeAttestationProof> {
            Ok(TeeAttestationProof { output: Bytes::new(), proof_bytes: Bytes::new() })
        }
    }

    #[tokio::test]
    async fn tdx_rejects_malformed_attestation_before_fetching_collateral() {
        let provider = HydratedTdxProofProvider::new(NoopProvider);

        assert!(provider.generate_proof_for_signer(b"malformed", Address::ZERO).await.is_err());
    }
}
