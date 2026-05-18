//! Core types and trait for attestation proof generation.

use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;

use crate::Result;

/// A generated attestation proof ready for on-chain submission.
#[derive(Debug, Clone)]
pub struct AttestationProof {
    /// ABI-encoded [`VerifierJournal`](base_proof_tee_nitro_verifier::VerifierJournal)
    /// containing the verified attestation data.
    pub output: Bytes,
    /// Groth16 seal bytes for on-chain verification.
    pub proof_bytes: Bytes,
}

/// Trait for generating ZK proofs over Nitro attestation documents.
///
/// Implementors wrap the attestation verification logic inside a ZK proving
/// backend and return a proof suitable for on-chain submission.
#[async_trait]
pub trait AttestationProofProvider: Send + Sync {
    /// Generates a ZK proof for the given raw attestation document bytes.
    async fn generate_proof(&self, attestation_bytes: &[u8]) -> Result<AttestationProof>;

    /// Generates a ZK proof with knowledge of the target signer address.
    ///
    /// Backends that support proof recovery (e.g. `BoundlessProver`, available
    /// with the `prove` feature) can use the signer address to derive
    /// deterministic request IDs and
    /// resume in-flight proofs after an instance rotation. The default
    /// implementation ignores the address and delegates to
    /// [`generate_proof`](Self::generate_proof).
    async fn generate_proof_for_signer(
        &self,
        attestation_bytes: &[u8],
        _signer_address: Address,
    ) -> Result<AttestationProof> {
        self.generate_proof(attestation_bytes).await
    }

    /// Marks a signer's recovered proof as failed on-chain.
    ///
    /// Called by the driver when a recovered proof for `signer` is rejected
    /// by the on-chain contract (e.g. `ExecutionReverted`). Implementations
    /// that support proof recovery should skip recovery for this signer on
    /// subsequent calls and instead generate a fresh proof.
    ///
    /// The default implementation is a no-op for backends that do not
    /// support recovery (e.g. `DirectProver`).
    fn block_recovery_for_signer(&self, _signer: Address) {}
}

#[async_trait]
impl AttestationProofProvider for Box<dyn AttestationProofProvider> {
    async fn generate_proof(&self, attestation_bytes: &[u8]) -> Result<AttestationProof> {
        (**self).generate_proof(attestation_bytes).await
    }

    async fn generate_proof_for_signer(
        &self,
        attestation_bytes: &[u8],
        signer_address: Address,
    ) -> Result<AttestationProof> {
        (**self).generate_proof_for_signer(attestation_bytes, signer_address).await
    }

    fn block_recovery_for_signer(&self, signer: Address) {
        (**self).block_recovery_for_signer(signer);
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn proof_fields_accessible() {
        let journal = Bytes::from_static(b"journal-data");
        let seal = Bytes::from_static(b"seal-data");

        let proof = AttestationProof { output: journal.clone(), proof_bytes: seal.clone() };

        assert_eq!(proof.output, journal);
        assert_eq!(proof.proof_bytes, seal);
    }

    #[rstest]
    fn proof_clone() {
        let proof = AttestationProof {
            output: Bytes::from_static(b"j"),
            proof_bytes: Bytes::from_static(b"s"),
        };
        let cloned = proof.clone();

        assert_eq!(proof.output, cloned.output);
        assert_eq!(proof.proof_bytes, cloned.proof_bytes);
    }

    #[rstest]
    fn proof_debug_format() {
        let proof = AttestationProof { output: Bytes::new(), proof_bytes: Bytes::new() };
        // Ensure Debug is implemented and doesn't panic.
        let debug = format!("{proof:?}");
        assert!(debug.contains("AttestationProof"));
    }

    // ── generate_proof_for_signer default impl ──────────────────────────

    /// Synthetic signer addresses for trait-delegation tests.
    const SIGNER_A: Address = Address::repeat_byte(0xAA);
    const SIGNER_B: Address = Address::repeat_byte(0xBB);

    /// Stub attestation input bytes.
    const STUB_ATTESTATION: &[u8] = b"attestation-data";

    /// Stub seal returned by [`StubProvider`].
    const STUB_SEAL: &[u8] = b"stub-seal";

    /// Stub provider whose `generate_proof` echoes the attestation
    /// bytes as `output` so callers can verify delegation happened.
    struct StubProvider;

    #[async_trait]
    impl AttestationProofProvider for StubProvider {
        async fn generate_proof(&self, attestation_bytes: &[u8]) -> Result<AttestationProof> {
            Ok(AttestationProof {
                output: Bytes::copy_from_slice(attestation_bytes),
                proof_bytes: Bytes::from_static(STUB_SEAL),
            })
        }
    }

    /// Provider that overrides `generate_proof_for_signer` to include the
    /// signer address in the output, proving the override is called
    /// instead of the default delegation.
    struct SignerAwareProvider;

    #[async_trait]
    impl AttestationProofProvider for SignerAwareProvider {
        async fn generate_proof(&self, _attestation_bytes: &[u8]) -> Result<AttestationProof> {
            panic!(
                "generate_proof should not be called when generate_proof_for_signer is overridden"
            );
        }

        async fn generate_proof_for_signer(
            &self,
            attestation_bytes: &[u8],
            signer_address: Address,
        ) -> Result<AttestationProof> {
            // Encode signer address into output to prove override was used.
            let mut output = attestation_bytes.to_vec();
            output.extend_from_slice(signer_address.as_slice());
            Ok(AttestationProof {
                output: Bytes::from(output),
                proof_bytes: Bytes::from_static(STUB_SEAL),
            })
        }
    }

    /// The default `generate_proof_for_signer` delegates to
    /// `generate_proof`, ignoring the signer address. Verified with
    /// multiple addresses to confirm the address has no effect.
    #[rstest]
    #[case::signer_a(SIGNER_A)]
    #[case::signer_b(SIGNER_B)]
    #[case::zero_address(Address::ZERO)]
    #[tokio::test]
    async fn default_generate_proof_for_signer_delegates_to_generate_proof(
        #[case] signer: Address,
    ) {
        let provider = StubProvider;
        let proof = provider.generate_proof_for_signer(STUB_ATTESTATION, signer).await.unwrap();

        assert_eq!(proof.output, Bytes::copy_from_slice(STUB_ATTESTATION));
        assert_eq!(proof.proof_bytes, Bytes::from_static(STUB_SEAL));
    }

    /// The `Box<dyn AttestationProofProvider>` impl forwards
    /// `generate_proof_for_signer` correctly.
    #[rstest]
    #[case::signer_a(SIGNER_A)]
    #[case::zero_address(Address::ZERO)]
    #[tokio::test]
    async fn boxed_provider_delegates_generate_proof_for_signer(#[case] signer: Address) {
        let provider: Box<dyn AttestationProofProvider> = Box::new(StubProvider);
        let proof = provider.generate_proof_for_signer(STUB_ATTESTATION, signer).await.unwrap();

        assert_eq!(proof.output, Bytes::copy_from_slice(STUB_ATTESTATION));
        assert_eq!(proof.proof_bytes, Bytes::from_static(STUB_SEAL));
    }

    /// When a provider overrides `generate_proof_for_signer`, the
    /// override is called instead of the default delegation.
    #[rstest]
    #[tokio::test]
    async fn custom_override_is_called_instead_of_default() {
        let provider = SignerAwareProvider;
        let proof = provider.generate_proof_for_signer(STUB_ATTESTATION, SIGNER_A).await.unwrap();

        // Output should contain attestation bytes + signer address,
        // proving the override ran (not the default delegation).
        let mut expected = STUB_ATTESTATION.to_vec();
        expected.extend_from_slice(SIGNER_A.as_slice());
        assert_eq!(proof.output, Bytes::from(expected));
    }

    /// A boxed provider with a custom override still dispatches to the
    /// override via dynamic dispatch.
    #[rstest]
    #[tokio::test]
    async fn boxed_custom_override_dispatches_correctly() {
        let provider: Box<dyn AttestationProofProvider> = Box::new(SignerAwareProvider);
        let proof = provider.generate_proof_for_signer(STUB_ATTESTATION, SIGNER_B).await.unwrap();

        let mut expected = STUB_ATTESTATION.to_vec();
        expected.extend_from_slice(SIGNER_B.as_slice());
        assert_eq!(proof.output, Bytes::from(expected));
    }
}
