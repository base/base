use core::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use base_proof_primitives::{ProofResult, ProverBackend};

use super::transport::NitroTransport;
use crate::{NitroError, Oracle};

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
    type Error = NitroError;

    fn create_oracle(&self) -> Oracle {
        Oracle::empty()
    }

    async fn prove(&self, witness: Oracle) -> Result<ProofResult, NitroError> {
        let preimages = witness.into_preimages()?;
        self.transport.prove(preimages).await
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_proof_preimage::{PreimageKey, WitnessOracle};
    use base_proof_primitives::ProverBackend;

    use super::*;
    use crate::enclave::{EnclaveConfig, Server};

    fn test_config() -> EnclaveConfig {
        EnclaveConfig { vsock_port: 0, config_hash: B256::ZERO, tee_image_hash: B256::ZERO }
    }

    #[tokio::test]
    async fn into_preimages_extracts_all_entries() {
        let oracle = Oracle::empty();

        let key = PreimageKey::new([2u8; 32], base_proof_preimage::PreimageKeyType::Local);
        oracle.insert_preimage(key, b"hello").unwrap();

        let preimages = oracle.into_preimages().unwrap();
        assert_eq!(preimages.len(), 1);
        assert_eq!(preimages[0], (key, b"hello".to_vec()));
    }

    #[tokio::test]
    async fn backend_create_oracle_returns_empty() {
        let config = test_config();
        let server = Arc::new(Server::new(&config).unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let backend = NitroBackend::new(transport);

        let oracle = backend.create_oracle();
        let preimages = oracle.into_preimages().unwrap();
        assert!(preimages.is_empty());
    }
}
