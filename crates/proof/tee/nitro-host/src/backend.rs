use core::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use base_proof_primitives::{ProofResult, ProverBackend};
use base_proof_tee_nitro_enclave::Oracle;

use super::transport::NitroTransport;
use crate::NitroHostError;

/// TEE proof backend that dispatches to a Nitro Enclave via [`NitroTransport`].
pub struct NitroBackend {
    transport: Arc<NitroTransport>,
}

impl fmt::Debug for NitroBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NitroBackend").finish_non_exhaustive()
    }
}

impl NitroBackend {
    /// Create a new backend dispatching proofs over the given transport.
    pub const fn new(transport: Arc<NitroTransport>) -> Self {
        Self { transport }
    }
}

#[async_trait]
impl ProverBackend for NitroBackend {
    type Oracle = Oracle;
    type Error = NitroHostError;

    fn create_oracle(&self) -> Oracle {
        Oracle::empty()
    }

    async fn prove(&self, witness: Oracle) -> Result<ProofResult, NitroHostError> {
        let preimages = witness.into_preimages().map_err(NitroHostError::Enclave)?;
        self.transport.prove(preimages).await
    }
}

#[cfg(test)]
mod tests {
    use base_proof_preimage::{PreimageKey, PreimageKeyType, WitnessOracle};
    use base_proof_tee_nitro_enclave::Server;

    use super::*;

    #[tokio::test]
    async fn into_preimages_extracts_all_entries() {
        let oracle = Oracle::empty();

        let key = PreimageKey::new([2u8; 32], PreimageKeyType::Local);
        oracle.insert_preimage(key, b"hello").unwrap();

        let preimages = oracle.into_preimages().unwrap();
        assert_eq!(preimages.len(), 1);
        assert_eq!(preimages[0], (key, b"hello".to_vec()));
    }

    #[tokio::test]
    async fn backend_create_oracle_returns_empty() {
        let server = Arc::new(Server::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let backend = NitroBackend::new(transport);

        let oracle = backend.create_oracle();
        let preimages = oracle.into_preimages().unwrap();
        assert!(preimages.is_empty());
    }
}
