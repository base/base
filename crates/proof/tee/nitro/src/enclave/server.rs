/// Enclave server — manages keys, attestation, signing, and proof execution.
use alloy_primitives::{Address, B256, Bytes, U256, b256, keccak256};
use alloy_signer_local::PrivateKeySigner;
use base_alloy_evm::OpEvmFactory;
use base_proof_client::{BootInfo, Prologue};
use base_proof_preimage::PreimageKey;
use base_proof_primitives::{ProofJournal, ProofResult, Proposal};
use tracing::info;

use crate::{
    Oracle,
    enclave::{
        crypto::{Ecdsa, Signing},
        nsm::{NsmRng, NsmSession},
    },
    error::{NitroError, NsmError, ProposalError, Result},
};

/// Environment variable for setting the signer key in local mode.
const SIGNER_KEY_ENV_VAR: &str = "OP_ENCLAVE_SIGNER_KEY";

/// PCR0 is a SHA-384 hash (48 bytes) per the AWS Nitro Enclaves specification.
const PCR0_LENGTH: usize = 48;

/// `keccak256(PerChainConfig::marshal_binary())` for Base Mainnet (chain 8453).
///
/// Produced by `print_real_config_hashes` in `base-enclave/src/types/config.rs`.
const CONFIG_HASH_BASE_MAINNET: B256 =
    b256!("1607709d90d40904f790574404e2ad614eac858f6162faa0ec34c6bf5e5f3c57");

/// `keccak256(PerChainConfig::marshal_binary())` for Base Sepolia (chain 84532).
///
/// Produced by `print_real_config_hashes` in `base-enclave/src/types/config.rs`.
const CONFIG_HASH_BASE_SEPOLIA: B256 =
    b256!("12e9c45f19f9817c6d4385fad29e7a70c355502cf0883e76a9a7e478a85d1360");

/// `keccak256(PerChainConfig::marshal_binary())` for Sepolia Alpha (chain 11763072).
///
/// Produced by `print_real_config_hashes` in `base-enclave/src/types/config.rs`.
const CONFIG_HASH_SEPOLIA_ALPHA: B256 =
    b256!("4600cdaa81262bf5f124bd9276f605264e2ded951e34923bc838e81c442f0fa4");

/// Look up the hardcoded config hash for a supported chain.
const fn config_hash_for_chain(chain_id: u64) -> Result<B256> {
    match chain_id {
        8453 => Ok(CONFIG_HASH_BASE_MAINNET),
        84532 => Ok(CONFIG_HASH_BASE_SEPOLIA),
        11763072 => Ok(CONFIG_HASH_SEPOLIA_ALPHA),
        _ => Err(NitroError::UnsupportedChain(chain_id)),
    }
}

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
    /// TEE image hash (keccak256 of PCR0 in enclave mode, zero in local mode).
    tee_image_hash: B256,
}

impl Server {
    /// Create a new server instance that requires NSM.
    ///
    /// Reads PCR0, keccak256-hashes it to derive `tee_image_hash`, and uses the
    /// hardware RNG for key generation. Returns an error if NSM is unavailable.
    pub fn new() -> Result<Self> {
        let session = NsmSession::open()?.ok_or_else(|| {
            NsmError::SessionOpen("NSM device unavailable; cannot run in enclave mode".into())
        })?;
        Self::new_enclave(&session)
    }

    /// Create a new server from an existing NSM session.
    pub fn new_enclave(session: &NsmSession) -> Result<Self> {
        let pcr0 = session.describe_pcr0()?;
        if pcr0.len() != PCR0_LENGTH {
            return Err(NsmError::DescribePcr(format!(
                "unexpected PCR0 length {}, expected {PCR0_LENGTH}",
                pcr0.len()
            ))
            .into());
        }

        let tee_image_hash = keccak256(&pcr0);

        let mut rng = NsmRng::new()
            .ok_or_else(|| NsmError::SessionOpen("failed to initialize NSM RNG".into()))?;
        let signer_key = Ecdsa::generate(&mut rng)?;

        Ok(Self { pcr0, signer_key, tee_image_hash })
    }

    /// Create a new server instance in local mode for development.
    ///
    /// Uses the OS RNG and sets `tee_image_hash` to zero. Optionally reads a
    /// signer key from the `OP_ENCLAVE_SIGNER_KEY` environment variable.
    pub fn new_local() -> Result<Self> {
        let signer_key = match std::env::var(SIGNER_KEY_ENV_VAR) {
            Ok(hex_key) => {
                info!("using signer key from environment variable");
                Ecdsa::from_hex(&hex_key)?
            }
            Err(_) => Ecdsa::generate(&mut NsmRng::default())?,
        };

        Ok(Self { pcr0: Vec::new(), signer_key, tee_image_hash: B256::ZERO })
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
    ///
    /// Optional `user_data` and `nonce` bind the attestation to a specific request.
    pub fn signer_attestation(
        &self,
        user_data: Option<Vec<u8>>,
        nonce: Option<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let session = NsmSession::open()?
            .ok_or_else(|| NsmError::SessionOpen("NSM not available".to_string()))?;
        let public_key = self.signer_public_key();
        session.get_attestation(public_key, user_data, nonce)
    }

    /// Run the proof-client pipeline for the given preimages and return per-block proposals
    /// with an aggregate.
    pub async fn prove(
        &self,
        preimages: impl IntoIterator<Item = (PreimageKey, Vec<u8>)>,
    ) -> Result<ProofResult> {
        let oracle = Oracle::new(preimages);

        let boot_info =
            BootInfo::load(&oracle).await.map_err(|e| NitroError::ProofPipeline(e.to_string()))?;
        let config_hash = config_hash_for_chain(boot_info.chain_id)?;
        let agreed_l2_output_root = boot_info.agreed_l2_output_root;

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
        let mut prev_output_root = agreed_l2_output_root;

        let l1_origin_hash = boot_info.l1_head;
        let l1_origin_number = boot_info.l1_head_number;
        for (l2_info, output_root) in &block_results {
            let l2_block_number = U256::from(l2_info.block_info.number);

            let journal = ProofJournal {
                proposer: boot_info.proposer,
                l1_origin_hash,
                prev_output_root,
                starting_l2_block: l2_block_number
                    .checked_sub(U256::from(1))
                    .ok_or_else(|| NitroError::ProofPipeline("l2_block_number is 0".into()))?,
                output_root: *output_root,
                ending_l2_block: l2_block_number,
                intermediate_roots: vec![],
                config_hash,
                tee_image_hash: self.tee_image_hash,
            };
            let signing_data = journal.encode();

            let signature = Signing::sign(&self.signer_key, &signing_data)?;

            proposals.push(Proposal {
                output_root: *output_root,
                signature: Bytes::from(signature.to_vec()),
                l1_origin_hash,
                l1_origin_number: U256::from(l1_origin_number),
                l2_block_number,
                prev_output_root,
                config_hash,
            });

            prev_output_root = *output_root;
        }

        let aggregate_proposal = if proposals.len() == 1 {
            proposals[0].clone()
        } else {
            let first = &proposals[0];
            let last = proposals.last().unwrap();

            let interval = boot_info.intermediate_block_interval;
            if interval == 0 {
                return Err(ProposalError::InvalidInterval.into());
            }
            let interval = interval as usize;
            let count = proposals.len() / interval;
            let intermediate_roots: Vec<B256> =
                (1..=count).map(|i| proposals[i * interval - 1].output_root).collect();

            let journal = ProofJournal {
                proposer: boot_info.proposer,
                l1_origin_hash,
                prev_output_root: agreed_l2_output_root,
                starting_l2_block: first
                    .l2_block_number
                    .checked_sub(U256::from(1))
                    .ok_or_else(|| NitroError::ProofPipeline("l2_block_number is 0".into()))?,
                output_root: last.output_root,
                ending_l2_block: last.l2_block_number,
                intermediate_roots,
                config_hash,
                tee_image_hash: self.tee_image_hash,
            };
            let signing_data = journal.encode();

            let signature = Signing::sign(&self.signer_key, &signing_data)?;

            Proposal {
                output_root: last.output_root,
                signature: Bytes::from(signature.to_vec()),
                l1_origin_hash,
                l1_origin_number: U256::from(l1_origin_number),
                l2_block_number: last.l2_block_number,
                prev_output_root: agreed_l2_output_root,
                config_hash,
            }
        };

        Ok(ProofResult::Tee { aggregate_proposal, proposals })
    }
}

#[cfg(test)]
mod tests {
    use base_consensus_registry::Registry;
    use base_enclave::PerChainConfig;

    use super::*;

    #[test]
    fn test_server_new_local_mode() {
        let server = Server::new_local().expect("failed to create server");
        assert!(server.is_local_mode());

        let public_key = server.signer_public_key();
        assert_eq!(public_key.len(), 65);
        assert_eq!(public_key[0], 0x04);
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn test_server_new_requires_nsm() {
        let result = Server::new();
        assert!(result.is_err());
    }

    #[test]
    fn test_signer_address_consistency() {
        let server = Server::new_local().expect("failed to create server");

        let addr1 = server.signer_address();
        let addr2 = server.signer_address();
        assert_eq!(addr1, addr2);

        let pk1 = server.signer_public_key();
        let pk2 = server.signer_public_key();
        assert_eq!(pk1, pk2);
    }

    #[test]
    fn config_hash_unknown_chain() {
        assert!(config_hash_for_chain(999999).is_err());
    }

    #[test]
    fn config_hashes_match_registry() {
        let chains: &[(u64, B256)] = &[
            (8453, CONFIG_HASH_BASE_MAINNET),
            (84532, CONFIG_HASH_BASE_SEPOLIA),
            (11763072, CONFIG_HASH_SEPOLIA_ALPHA),
        ];

        for &(chain_id, expected) in chains {
            let rollup = Registry::rollup_config(chain_id)
                .unwrap_or_else(|| panic!("missing rollup config for chain {chain_id}"));
            let mut per_chain = PerChainConfig::from_rollup_config(rollup)
                .unwrap_or_else(|| panic!("missing system_config for chain {chain_id}"));
            per_chain.force_defaults();

            assert_eq!(per_chain.hash(), expected, "config hash mismatch for chain {chain_id}");
        }
    }
}
