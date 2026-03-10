/// Enclave server — manages keys, attestation, signing, and proof execution.
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_signer_local::PrivateKeySigner;
use base_alloy_evm::OpEvmFactory;
use base_proof_client::Prologue;
use base_proof_primitives::{ProofBundle, ProofClaim, ProofEvidence, ProofResult, Proposal};
use tracing::{info, warn};

use crate::{
    Oracle,
    enclave::{
        EnclaveConfig,
        crypto::{Ecdsa, Signing},
        nsm::{NsmRng, NsmSession},
    },
    error::{NitroError, NsmError, ProposalError, Result},
};

/// Environment variable for setting the signer key in local mode.
const SIGNER_KEY_ENV_VAR: &str = "OP_ENCLAVE_SIGNER_KEY";

/// PCR0 length
const PCR0_LENGTH: usize = 32;

/// The enclave server.
///
/// Manages cryptographic keys and attestation for the enclave.
/// Supports both Nitro Enclave mode (with NSM) and local mode (for development).
#[derive(Debug)]
pub struct Server {
    /// PCR0 measurement (empty in local mode).
    pcr0: Vec<u8>,
    /// ECDSA signing key.
    signer_key: PrivateKeySigner,
    /// The proposer address.
    proposer: Address,
    /// Per-chain config hash.
    config_hash: B256,
    /// TEE image hash (from PCR0 or config in local mode).
    tee_image_hash: B256,
}

impl Server {
    /// Create a new server instance.
    ///
    /// Attempts to open an NSM session and verify PCR0 against `config.tee_image_hash`.
    /// Falls back to local mode if NSM is unavailable.
    pub fn new(config: &EnclaveConfig) -> Result<Self> {
        let (mut rng, pcr0) = match NsmSession::open()? {
            Some(session) => {
                let pcr0 = session.describe_pcr0()?;

                if pcr0.len() != PCR0_LENGTH {
                    return Err(NsmError::DescribePcr(format!(
                        "unexpected PCR0 length {}, expected 32.",
                        pcr0.len()
                    ))
                    .into());
                }
                let actual_hash = B256::from_slice(&pcr0);
                if actual_hash != config.tee_image_hash {
                    return Err(NitroError::Pcr0Mismatch {
                        expected: config.tee_image_hash,
                        actual: actual_hash,
                    });
                }

                let rng = NsmRng::new()
                    .ok_or_else(|| NsmError::SessionOpen("failed to initialize NSM RNG".into()))?;
                (rng, pcr0)
            }
            None => {
                warn!("running in local mode without NSM");
                (NsmRng::default(), Vec::new())
            }
        };

        let signer_key = match std::env::var(SIGNER_KEY_ENV_VAR) {
            Ok(hex_key) => {
                info!("using signer key from environment variable");
                Ecdsa::from_hex(&hex_key)?
            }
            Err(_) => Ecdsa::generate(&mut rng)?,
        };

        let tee_image_hash =
            if pcr0.is_empty() { config.tee_image_hash } else { B256::from_slice(&pcr0) };

        Ok(Self {
            pcr0,
            signer_key,
            proposer: config.proposer,
            config_hash: config.config_hash,
            tee_image_hash,
        })
    }

    /// Check if the server is running in local mode.
    #[must_use]
    pub const fn is_local_mode(&self) -> bool {
        self.pcr0.is_empty()
    }

    /// Get the signer's public key as a 65-byte uncompressed EC point.
    #[must_use]
    pub fn signer_public_key(&self) -> Vec<u8> {
        Ecdsa::public_key_bytes(&self.signer_key)
    }

    /// Get the signer's Ethereum address.
    #[must_use]
    pub const fn signer_address(&self) -> Address {
        self.signer_key.address()
    }

    /// Get an attestation document containing the signer's public key.
    pub fn signer_attestation(&self) -> Result<Vec<u8>> {
        let session = NsmSession::open()?
            .ok_or_else(|| NsmError::SessionOpen("NSM not available".to_string()))?;
        let public_key = self.signer_public_key();
        session.get_attestation(public_key)
    }

    /// Try to get attestation bytes, returning empty vec on failure.
    fn try_get_attestation_bytes(&self) -> Vec<u8> {
        self.signer_attestation().unwrap_or_default()
    }

    /// Run the proof-client pipeline for a proof bundle and return per-block proposals
    /// with an aggregate.
    pub async fn prove(&self, bundle: ProofBundle) -> Result<ProofResult> {
        let ProofBundle { request, preimages } = bundle;
        let oracle = Oracle::new(preimages);

        let prologue = Prologue::new(oracle.clone(), oracle, OpEvmFactory::default());
        let driver = prologue.load().await.map_err(|e| NitroError::ProofPipeline(e.to_string()))?;
        let (epilogue, block_results) = driver
            .execute_with_intermediates()
            .await
            .map_err(|e| NitroError::ProofPipeline(e.to_string()))?;

        if block_results.is_empty() {
            return Err(ProposalError::EmptyProposals.into());
        }

        // Trust-critical: validate final output root against claim
        epilogue.validate().map_err(|e| NitroError::ProofPipeline(e.to_string()))?;

        let mut proposals = Vec::with_capacity(block_results.len());
        let mut prev_output_root = request.agreed_l2_output_root;

        for (l2_info, output_root) in &block_results {
            let l2_block_number = U256::from(l2_info.block_info.number);
            let l1_origin_hash = l2_info.l1_origin.hash;
            let l1_origin_number = U256::from(l2_info.l1_origin.number);

            let signing_data = Signing::build_data(
                self.proposer,
                l1_origin_hash,
                prev_output_root,
                l2_block_number
                    .checked_sub(U256::from(1))
                    .ok_or_else(|| NitroError::ProofPipeline("l2_block_number is 0".into()))?,
                *output_root,
                l2_block_number,
                &[],
                self.config_hash,
                self.tee_image_hash,
            );

            let signature = Signing::sign(&self.signer_key, &signing_data)?;

            proposals.push(Proposal {
                output_root: *output_root,
                signature: Bytes::from(signature.to_vec()),
                l1_origin_hash,
                l1_origin_number,
                l2_block_number,
                prev_output_root,
                config_hash: self.config_hash,
            });

            prev_output_root = *output_root;
        }

        let aggregate_proposal = if proposals.len() == 1 {
            proposals[0].clone()
        } else {
            let first = &proposals[0];
            let last = proposals.last().unwrap();

            let intermediate_roots: Vec<B256> =
                proposals[..proposals.len() - 1].iter().map(|p| p.output_root).collect();

            let signing_data = Signing::build_data(
                self.proposer,
                last.l1_origin_hash,
                request.agreed_l2_output_root,
                first
                    .l2_block_number
                    .checked_sub(U256::from(1))
                    .ok_or_else(|| NitroError::ProofPipeline("l2_block_number is 0".into()))?,
                last.output_root,
                last.l2_block_number,
                &intermediate_roots,
                self.config_hash,
                self.tee_image_hash,
            );

            let signature = Signing::sign(&self.signer_key, &signing_data)?;

            Proposal {
                output_root: last.output_root,
                signature: Bytes::from(signature.to_vec()),
                l1_origin_hash: last.l1_origin_hash,
                l1_origin_number: last.l1_origin_number,
                l2_block_number: last.l2_block_number,
                prev_output_root: request.agreed_l2_output_root,
                config_hash: self.config_hash,
            }
        };

        let attestation_doc = self.try_get_attestation_bytes();
        let evidence_signature = Signing::sign(&self.signer_key, &aggregate_proposal.signature)?;

        Ok(ProofResult {
            claim: ProofClaim { aggregate_proposal, proposals },
            evidence: ProofEvidence::Tee {
                attestation_doc,
                signature: evidence_signature.to_vec(),
            },
        })
    }

    /// Create a server for testing (no NSM, no PCR0 verification).
    #[cfg(test)]
    pub fn new_for_testing(config: &EnclaveConfig) -> Result<Self> {
        let signer_key = Ecdsa::generate(&mut rand_08::rngs::OsRng)?;
        Ok(Self {
            pcr0: Vec::new(),
            signer_key,
            proposer: config.proposer,
            config_hash: config.config_hash,
            tee_image_hash: config.tee_image_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> EnclaveConfig {
        EnclaveConfig {
            vsock_port: 1234,
            proposer: Address::ZERO,
            config_hash: B256::ZERO,
            tee_image_hash: B256::ZERO,
        }
    }

    #[test]
    fn test_server_new_local_mode() {
        let config = test_config();
        let server = Server::new(&config).expect("failed to create server");

        #[cfg(not(target_os = "linux"))]
        assert!(server.is_local_mode());

        let public_key = server.signer_public_key();
        assert_eq!(public_key.len(), 65);
        assert_eq!(public_key[0], 0x04);
    }

    #[test]
    fn test_signer_address_consistency() {
        let config = test_config();
        let server = Server::new(&config).expect("failed to create server");

        let addr1 = server.signer_address();
        let addr2 = server.signer_address();
        assert_eq!(addr1, addr2);

        let pk1 = server.signer_public_key();
        let pk2 = server.signer_public_key();
        assert_eq!(pk1, pk2);
    }
}
