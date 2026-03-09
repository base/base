use core::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use base_proof_primitives::{ProofResult, ProverBackend};
use base_proof_transport::ProofTransport;

use crate::{NitroError, Oracle};

/// TEE proof backend that dispatches to a Nitro Enclave via [`ProofTransport`].
pub struct NitroBackend {
    transport: Arc<dyn ProofTransport>,
}

impl fmt::Debug for NitroBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NitroBackend").finish_non_exhaustive()
    }
}

impl NitroBackend {
    /// Create a new backend dispatching proofs over the given transport.
    pub fn new(transport: Arc<dyn ProofTransport>) -> Self {
        Self { transport }
    }
}

#[async_trait]
impl ProverBackend for NitroBackend {
    type Oracle = Oracle;
    type Error = NitroError;

    fn create_oracle(&self) -> Oracle {
        Oracle::empty()
    }

    async fn prove(&self, witness: Oracle) -> Result<ProofResult, NitroError> {
        let preimages = witness.into_preimages();
        self.transport.prove(&preimages).await.map_err(|e| NitroError::Transport(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U256};
    use base_proof_preimage::{PreimageKey, WitnessOracle};
    use base_proof_primitives::{ProofClaim, ProofEvidence, ProofResult, Proposal, ProverBackend};
    use base_proof_transport::NativeTransport;

    use super::*;

    fn test_proposal() -> Proposal {
        Proposal {
            output_root: Default::default(),
            signature: Bytes::from(vec![0u8; 65]),
            l1_origin_hash: Default::default(),
            l1_origin_number: U256::ZERO,
            l2_block_number: U256::ZERO,
            prev_output_root: Default::default(),
            config_hash: Default::default(),
        }
    }

    fn test_result() -> ProofResult {
        ProofResult {
            claim: ProofClaim { aggregate_proposal: test_proposal(), proposals: vec![] },
            evidence: ProofEvidence::Tee { attestation_doc: vec![], signature: vec![] },
        }
    }

    #[tokio::test]
    async fn prove_sends_preimages_via_transport() {
        let expected = test_result();
        let expected_clone = expected.clone();

        let transport = NativeTransport::new(move |_preimages| expected_clone.clone());
        let backend = NitroBackend::new(Arc::new(transport));

        let oracle = backend.create_oracle();

        let key = PreimageKey::new([1u8; 32], base_proof_preimage::PreimageKeyType::Local);
        oracle.insert_preimage(key, b"test_value").unwrap();

        let result = backend.prove(oracle).await.unwrap();
        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn into_preimages_extracts_all_entries() {
        let oracle = Oracle::empty();

        let key = PreimageKey::new([2u8; 32], base_proof_preimage::PreimageKeyType::Local);
        oracle.insert_preimage(key, b"hello").unwrap();

        let preimages = oracle.into_preimages();
        assert_eq!(preimages.len(), 1);
        assert_eq!(preimages[0], (key, b"hello".to_vec()));
    }
}
