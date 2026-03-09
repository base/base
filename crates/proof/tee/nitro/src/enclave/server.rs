/// Enclave server — manages keys, attestation, signing, and proof execution.
use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use base_alloy_evm::OpEvmFactory;
use base_enclave::{Proposal, ProposalParams};
use base_proof_client::{Epilogue, Prologue};
use base_proof_primitives::{ProofBundle, ProofClaim, ProofEvidence, ProofResult};
use parking_lot::RwLock;
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
    signer_key: RwLock<PrivateKeySigner>,
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
            signer_key: RwLock::new(signer_key),
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
        let signer = self.signer_key.read();
        Ecdsa::public_key_bytes(&signer)
    }

    /// Get the signer's Ethereum address.
    #[must_use]
    pub fn signer_address(&self) -> Address {
        let signer = self.signer_key.read();
        signer.address()
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

    /// Run the proof-client pipeline for a proof bundle.
    pub async fn prove(&self, bundle: ProofBundle) -> Result<ProofResult> {
        let ProofBundle { request, preimages } = bundle;
        let oracle = Oracle::new(preimages);

        // Run proof-client pipeline
        let prologue = Prologue::new(oracle.clone(), oracle, OpEvmFactory::default());
        let driver = prologue.load().await.map_err(|e| NitroError::ProofPipeline(e.to_string()))?;
        let epilogue: Epilogue =
            driver.execute().await.map_err(|e| NitroError::ProofPipeline(e.to_string()))?;

        let output_root = epilogue.output_root;
        let l2_block_number = epilogue.safe_head.block_info.number;

        // Trust-critical check inside TEE
        epilogue.validate().map_err(|e| NitroError::ProofPipeline(e.to_string()))?;

        // Sign using AggregateVerifier journal format
        let signing_data = Signing::build_data(
            self.proposer,
            request.l1_head,
            request.agreed_l2_output_root,
            U256::from(l2_block_number.checked_sub(1).ok_or_else(|| {
                NitroError::ProofPipeline("l2_block_number is 0, cannot compute starting block".into())
            })?),
            output_root,
            U256::from(l2_block_number),
            &[],
            self.config_hash,
            self.tee_image_hash,
        );

        let signer = self.signer_key.read();
        let signature = Signing::sign(&signer, &signing_data)?;
        drop(signer);
        let attestation_doc = self.try_get_attestation_bytes();

        Ok(ProofResult {
            claim: ProofClaim { l2_block_number, output_root, l1_head: request.l1_head },
            evidence: ProofEvidence::Tee { attestation_doc, signature: signature.to_vec() },
        })
    }

    /// Aggregate multiple proposals into a single proposal.
    ///
    /// Verifies the signature chain and creates a new aggregated proposal.
    #[allow(clippy::too_many_arguments)]
    pub fn aggregate(
        &self,
        config_hash: B256,
        prev_output_root: B256,
        prev_block_number: u64,
        proposals: &[Proposal],
        proposer: Address,
        tee_image_hash: B256,
        intermediate_roots: &[B256],
    ) -> Result<Proposal> {
        if proposals.is_empty() {
            return Err(ProposalError::EmptyProposals.into());
        }

        if proposals.len() == 1 {
            if !intermediate_roots.is_empty() {
                return Err(ProposalError::ExecutionFailed(
                    "intermediate roots must be empty for single-proposal aggregation".to_string(),
                )
                .into());
            }
            return Ok(proposals[0].clone());
        }

        let public_key = self.signer_public_key();

        let mut output_root = prev_output_root;
        let mut l1_origin_hash = B256::ZERO;
        let mut l1_origin_number = U256::ZERO;
        let mut l2_block_number = U256::ZERO;
        let mut prev_l2_block = U256::from(prev_block_number);

        for (index, proposal) in proposals.iter().enumerate() {
            l1_origin_hash = proposal.l1_origin_hash;
            l1_origin_number = proposal.l1_origin_number;
            l2_block_number = proposal.l2_block_number;

            let signing_data = Signing::build_data(
                proposer,
                l1_origin_hash,
                output_root,
                prev_l2_block,
                proposal.output_root,
                l2_block_number,
                &[],
                config_hash,
                tee_image_hash,
            );

            match Signing::verify(&public_key, &signing_data, &proposal.signature) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(ProposalError::InvalidSignature {
                        index,
                        reason: "signature mismatch".to_string(),
                    }
                    .into());
                }
                Err(e) => {
                    return Err(
                        ProposalError::InvalidSignature { index, reason: e.to_string() }.into()
                    );
                }
            }

            output_root = proposal.output_root;
            prev_l2_block = l2_block_number;
        }

        // Validate intermediate roots against proposals
        if !intermediate_roots.is_empty() && proposals.len() > 1 {
            let last_block = proposals.last().unwrap().l2_block_number.to::<u64>();
            let total_blocks = last_block.checked_sub(prev_block_number).ok_or_else(|| {
                ProposalError::ExecutionFailed(format!(
                    "last block ({last_block}) is before previous block ({prev_block_number})"
                ))
            })?;
            if !total_blocks.is_multiple_of(intermediate_roots.len() as u64) {
                return Err(ProposalError::ExecutionFailed(format!(
                    "block range ({total_blocks}) not divisible by intermediate root count ({})",
                    intermediate_roots.len()
                ))
                .into());
            }
            let interval = total_blocks / intermediate_roots.len() as u64;

            for (i, expected_root) in intermediate_roots.iter().enumerate() {
                let target_block = U256::from(prev_block_number + (i as u64 + 1) * interval);
                match proposals.iter().find(|p| p.l2_block_number == target_block) {
                    Some(p) if p.output_root == *expected_root => {}
                    Some(p) => {
                        return Err(ProposalError::InvalidIntermediateRoot {
                            index: i,
                            expected: format!("{:?}", p.output_root),
                            actual: format!("{expected_root:?}"),
                        }
                        .into());
                    }
                    None => {
                        return Err(ProposalError::MissingIntermediateProposal {
                            block: target_block.to::<u64>(),
                        }
                        .into());
                    }
                }
            }
        }

        let final_output_root = output_root;
        let starting_l2_block = U256::from(prev_block_number);

        let signing_data = Signing::build_data(
            proposer,
            l1_origin_hash,
            prev_output_root,
            starting_l2_block,
            final_output_root,
            l2_block_number,
            intermediate_roots,
            config_hash,
            tee_image_hash,
        );

        let signer = self.signer_key.read();
        let signature = Signing::sign(&signer, &signing_data)?;

        Ok(Proposal::new(ProposalParams {
            output_root: final_output_root,
            signature,
            l1_origin_hash,
            l1_origin_number,
            l2_block_number,
            prev_output_root,
            config_hash,
        }))
    }

    /// Get a read guard for the signer.
    pub fn signer(&self) -> parking_lot::RwLockReadGuard<'_, PrivateKeySigner> {
        self.signer_key.read()
    }

    /// Create a server for testing (no NSM, no PCR0 verification).
    #[cfg(test)]
    pub fn new_for_testing(config: &EnclaveConfig) -> Result<Self> {
        let signer_key = Ecdsa::generate(&mut rand_08::rngs::OsRng)?;
        Ok(Self {
            pcr0: Vec::new(),
            signer_key: RwLock::new(signer_key),
            proposer: config.proposer,
            config_hash: config.config_hash,
            tee_image_hash: config.tee_image_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::slice;

    use alloy_primitives::Bytes;

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

    #[test]
    fn test_aggregate_empty_proposals() {
        let config = test_config();
        let server = Server::new_for_testing(&config).expect("failed to create server");

        let result =
            server.aggregate(B256::ZERO, B256::ZERO, 99u64, &[], Address::ZERO, B256::ZERO, &[]);

        assert!(matches!(result, Err(NitroError::Proposal(ProposalError::EmptyProposals))));
    }

    #[test]
    fn test_aggregate_single_proposal() {
        let config = test_config();
        let server = Server::new_for_testing(&config).expect("failed to create server");

        let config_hash = B256::repeat_byte(0x11);
        let l1_origin_hash = B256::repeat_byte(0x22);
        let l1_origin_number = U256::from(100);
        let l2_block_number = U256::from(100);
        let prev_output_root = B256::repeat_byte(0x33);
        let output_root = B256::repeat_byte(0x44);
        let proposer = Address::ZERO;
        let tee_image_hash = B256::ZERO;
        let prev_block_number = 99u64;

        let signing_data = Signing::build_data(
            proposer,
            l1_origin_hash,
            prev_output_root,
            U256::from(prev_block_number),
            output_root,
            l2_block_number,
            &[],
            config_hash,
            tee_image_hash,
        );

        let signer = server.signer();
        let signature = Signing::sign(&signer, &signing_data).expect("signing failed");
        drop(signer);

        let proposal = Proposal::new(ProposalParams {
            output_root,
            signature,
            l1_origin_hash,
            l1_origin_number,
            l2_block_number,
            prev_output_root,
            config_hash,
        });

        let result = server.aggregate(
            config_hash,
            prev_output_root,
            prev_block_number,
            slice::from_ref(&proposal),
            proposer,
            tee_image_hash,
            &[],
        );

        assert!(result.is_ok());
        let aggregated = result.unwrap();
        assert_eq!(aggregated, proposal);
    }

    #[test]
    fn test_aggregate_multiple_proposals() {
        let config = test_config();
        let server = Server::new_for_testing(&config).expect("failed to create server");

        let config_hash = B256::repeat_byte(0x11);
        let l1_origin_hash_1 = B256::repeat_byte(0x22);
        let l1_origin_hash_2 = B256::repeat_byte(0x23);
        let l1_origin_number = U256::from(100);
        let prev_output_root = B256::repeat_byte(0x33);
        let output_root_1 = B256::repeat_byte(0x44);
        let output_root_2 = B256::repeat_byte(0x55);
        let proposer = Address::ZERO;
        let tee_image_hash = B256::ZERO;
        let prev_block_number = 99u64;

        let signing_data_1 = Signing::build_data(
            proposer,
            l1_origin_hash_1,
            prev_output_root,
            U256::from(prev_block_number),
            output_root_1,
            U256::from(100),
            &[],
            config_hash,
            tee_image_hash,
        );

        let signer = server.signer();
        let signature_1 = Signing::sign(&signer, &signing_data_1).expect("signing failed");
        drop(signer);

        let proposal_1 = Proposal::new(ProposalParams {
            output_root: output_root_1,
            signature: signature_1,
            l1_origin_hash: l1_origin_hash_1,
            l1_origin_number,
            l2_block_number: U256::from(100),
            prev_output_root,
            config_hash,
        });

        let signing_data_2 = Signing::build_data(
            proposer,
            l1_origin_hash_2,
            output_root_1,
            U256::from(100),
            output_root_2,
            U256::from(101),
            &[],
            config_hash,
            tee_image_hash,
        );

        let signer = server.signer();
        let signature_2 = Signing::sign(&signer, &signing_data_2).expect("signing failed");
        drop(signer);

        let proposal_2 = Proposal::new(ProposalParams {
            output_root: output_root_2,
            signature: signature_2,
            l1_origin_hash: l1_origin_hash_2,
            l1_origin_number,
            l2_block_number: U256::from(101),
            prev_output_root: output_root_1,
            config_hash,
        });

        let result = server.aggregate(
            config_hash,
            prev_output_root,
            prev_block_number,
            &[proposal_1, proposal_2],
            proposer,
            tee_image_hash,
            &[],
        );

        assert!(result.is_ok());
        let aggregated = result.unwrap();
        assert_eq!(aggregated.output_root, output_root_2);
        assert_eq!(aggregated.prev_output_root, prev_output_root);
        assert_eq!(aggregated.l1_origin_hash, l1_origin_hash_2);
        assert_eq!(aggregated.l2_block_number, U256::from(101));
        assert_eq!(aggregated.config_hash, config_hash);
    }

    #[test]
    fn test_aggregate_invalid_signature_rejected() {
        let config = test_config();
        let server = Server::new_for_testing(&config).expect("failed to create server");

        let config_hash = B256::repeat_byte(0x11);
        let l1_origin_hash_1 = B256::repeat_byte(0x22);
        let l1_origin_hash_2 = B256::repeat_byte(0x23);
        let l1_origin_number = U256::from(100);
        let prev_output_root = B256::repeat_byte(0x33);
        let output_root_1 = B256::repeat_byte(0x44);
        let output_root_2 = B256::repeat_byte(0x55);
        let proposer = Address::ZERO;
        let tee_image_hash = B256::ZERO;
        let prev_block_number = 99u64;

        let signing_data_1 = Signing::build_data(
            proposer,
            l1_origin_hash_1,
            prev_output_root,
            U256::from(prev_block_number),
            output_root_1,
            U256::from(100),
            &[],
            config_hash,
            tee_image_hash,
        );

        let signer = server.signer();
        let signature_1 = Signing::sign(&signer, &signing_data_1).expect("signing failed");
        drop(signer);

        let proposal_1 = Proposal::new(ProposalParams {
            output_root: output_root_1,
            signature: signature_1,
            l1_origin_hash: l1_origin_hash_1,
            l1_origin_number,
            l2_block_number: U256::from(100),
            prev_output_root,
            config_hash,
        });

        let invalid_signature = Bytes::from(vec![0xab; 65]);

        let proposal_2 = Proposal::new(ProposalParams {
            output_root: output_root_2,
            signature: invalid_signature,
            l1_origin_hash: l1_origin_hash_2,
            l1_origin_number,
            l2_block_number: U256::from(101),
            prev_output_root: output_root_1,
            config_hash,
        });

        let result = server.aggregate(
            config_hash,
            prev_output_root,
            prev_block_number,
            &[proposal_1, proposal_2],
            proposer,
            tee_image_hash,
            &[],
        );

        assert!(
            matches!(
                &result,
                Err(NitroError::Proposal(ProposalError::InvalidSignature { index: 1, .. }))
            ),
            "Expected InvalidSignature error at index 1, got: {result:?}"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn make_proposal(
        server: &Server,
        proposer: Address,
        tee_image_hash: B256,
        config_hash: B256,
        l1_origin_hash: B256,
        l1_origin_number: U256,
        prev_output_root: B256,
        prev_block: u64,
        output_root: B256,
        block: u64,
    ) -> Proposal {
        let signing_data = Signing::build_data(
            proposer,
            l1_origin_hash,
            prev_output_root,
            U256::from(prev_block),
            output_root,
            U256::from(block),
            &[],
            config_hash,
            tee_image_hash,
        );
        let signer = server.signer();
        let signature = Signing::sign(&signer, &signing_data).expect("signing failed");
        drop(signer);

        Proposal::new(ProposalParams {
            output_root,
            signature,
            l1_origin_hash,
            l1_origin_number,
            l2_block_number: U256::from(block),
            prev_output_root,
            config_hash,
        })
    }

    #[test]
    fn test_aggregate_validates_intermediate_roots() {
        let config = test_config();
        let server = Server::new_for_testing(&config).expect("failed to create server");

        let proposer = Address::ZERO;
        let tee_image_hash = B256::ZERO;
        let config_hash = B256::repeat_byte(0x11);
        let l1_origin_hash = B256::repeat_byte(0x22);
        let l1_origin_number = U256::from(100);
        let prev_output_root = B256::repeat_byte(0x33);
        let output_root_1 = B256::repeat_byte(0x44);
        let output_root_2 = B256::repeat_byte(0x55);
        let output_root_3 = B256::repeat_byte(0x66);
        let prev_block_number = 0u64;

        let p1 = make_proposal(
            &server,
            proposer,
            tee_image_hash,
            config_hash,
            l1_origin_hash,
            l1_origin_number,
            prev_output_root,
            0,
            output_root_1,
            1,
        );
        let p2 = make_proposal(
            &server,
            proposer,
            tee_image_hash,
            config_hash,
            l1_origin_hash,
            l1_origin_number,
            output_root_1,
            1,
            output_root_2,
            2,
        );
        let p3 = make_proposal(
            &server,
            proposer,
            tee_image_hash,
            config_hash,
            l1_origin_hash,
            l1_origin_number,
            output_root_2,
            2,
            output_root_3,
            3,
        );

        let result = server.aggregate(
            config_hash,
            prev_output_root,
            prev_block_number,
            &[p1, p2, p3],
            proposer,
            tee_image_hash,
            &[output_root_3],
        );

        assert!(result.is_ok(), "expected success, got: {result:?}");
    }

    #[test]
    fn test_aggregate_rejects_wrong_intermediate_roots() {
        let config = test_config();
        let server = Server::new_for_testing(&config).expect("failed to create server");

        let proposer = Address::ZERO;
        let tee_image_hash = B256::ZERO;
        let config_hash = B256::repeat_byte(0x11);
        let l1_origin_hash = B256::repeat_byte(0x22);
        let l1_origin_number = U256::from(100);
        let prev_output_root = B256::repeat_byte(0x33);
        let output_root_1 = B256::repeat_byte(0x44);
        let output_root_2 = B256::repeat_byte(0x55);
        let output_root_3 = B256::repeat_byte(0x66);
        let prev_block_number = 0u64;

        let p1 = make_proposal(
            &server,
            proposer,
            tee_image_hash,
            config_hash,
            l1_origin_hash,
            l1_origin_number,
            prev_output_root,
            0,
            output_root_1,
            1,
        );
        let p2 = make_proposal(
            &server,
            proposer,
            tee_image_hash,
            config_hash,
            l1_origin_hash,
            l1_origin_number,
            output_root_1,
            1,
            output_root_2,
            2,
        );
        let p3 = make_proposal(
            &server,
            proposer,
            tee_image_hash,
            config_hash,
            l1_origin_hash,
            l1_origin_number,
            output_root_2,
            2,
            output_root_3,
            3,
        );

        let wrong_root = B256::repeat_byte(0xFF);
        let result = server.aggregate(
            config_hash,
            prev_output_root,
            prev_block_number,
            &[p1, p2, p3],
            proposer,
            tee_image_hash,
            &[wrong_root],
        );

        assert!(
            matches!(
                &result,
                Err(NitroError::Proposal(ProposalError::InvalidIntermediateRoot { .. }))
            ),
            "Expected InvalidIntermediateRoot error, got: {result:?}"
        );
    }

    #[test]
    fn test_aggregate_rejects_intermediate_roots_with_single_proposal() {
        let config = test_config();
        let server = Server::new_for_testing(&config).expect("failed to create server");

        let proposer = Address::ZERO;
        let tee_image_hash = B256::ZERO;
        let config_hash = B256::repeat_byte(0x11);
        let l1_origin_hash = B256::repeat_byte(0x22);
        let l1_origin_number = U256::from(100);
        let prev_output_root = B256::repeat_byte(0x33);
        let output_root = B256::repeat_byte(0x44);
        let prev_block_number = 99u64;

        let proposal = make_proposal(
            &server,
            proposer,
            tee_image_hash,
            config_hash,
            l1_origin_hash,
            l1_origin_number,
            prev_output_root,
            prev_block_number,
            output_root,
            100,
        );

        let result = server.aggregate(
            config_hash,
            prev_output_root,
            prev_block_number,
            &[proposal],
            proposer,
            tee_image_hash,
            &[B256::repeat_byte(0xFF)],
        );

        assert!(
            matches!(&result, Err(NitroError::Proposal(ProposalError::ExecutionFailed(_)))),
            "Expected ExecutionFailed error for single proposal with intermediate roots, got: {result:?}"
        );
    }
}
