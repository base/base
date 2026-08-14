/// Enclave server — manages keys, attestation, signing, and proof execution.
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_signer_local::PrivateKeySigner;
use base_common_evm::BaseEvmFactory;
use base_common_genesis::RollupConfig;
use base_proof::BootInfo;
use base_proof_client::Prologue;
use base_proof_preimage::PreimageKey;
use base_proof_primitives::{
    ATTESTED_WITHDRAWAL_SLOT, PerChainConfig, ProofJournal, ProofResult, Proposal,
    StorageProofVerifier, WithdrawalAuthorization,
};
use tracing::info;

use crate::{
    Ecdsa, NsmRng, NsmSession, Oracle, Signing,
    error::{NitroError, NsmError, ProposalError, Result},
};

/// Environment variable for setting the signer key in local mode.
const SIGNER_KEY_ENV_VAR: &str = "BASE_ENCLAVE_SIGNER_KEY";

/// PCR0 is a SHA-384 hash (48 bytes) per the AWS Nitro Enclaves specification.
const PCR0_LENGTH: usize = 48;

/// Compute the config hash from a [`RollupConfig`].
///
/// Builds a [`PerChainConfig`] from the rollup config, applies forced defaults,
/// and returns the `keccak256` hash of its canonical binary encoding.
///
/// This replaces the previous hardcoded `CONFIG_HASHES` table so that chains
/// not compiled into `ChainConfig::all()` (e.g. devnets) are supported.
fn config_hash(rollup_config: &RollupConfig) -> Result<B256> {
    let chain_id = rollup_config.l2_chain_id.id();
    let mut per_chain = PerChainConfig::from_rollup_config(rollup_config)
        .ok_or(NitroError::MissingSystemConfig(chain_id))?;
    per_chain.force_defaults();
    Ok(per_chain.hash())
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
    /// signer key from the `BASE_ENCLAVE_SIGNER_KEY` environment variable.
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

    /// Verify a withdrawal record in the message passer storage trie and sign it.
    ///
    /// # Errors
    ///
    /// Returns an error when the storage proof does not establish the withdrawal
    /// record or the enclave cannot produce a signature.
    pub fn sign_attested_withdrawal(
        &self,
        auth_hash: B256,
        message_passer_storage_root: B256,
        storage_proof: &[Bytes],
    ) -> Result<Bytes> {
        let mut encoded_slot = [0_u8; 64];
        encoded_slot[..32].copy_from_slice(auth_hash.as_slice());
        encoded_slot[56..].copy_from_slice(&ATTESTED_WITHDRAWAL_SLOT.to_be_bytes());
        let storage_slot = keccak256(encoded_slot);

        StorageProofVerifier::verify_storage_slot(
            message_passer_storage_root,
            storage_slot,
            &[0x01],
            storage_proof,
        )
        .map_err(|error| NitroError::ProofPipeline(error.to_string()))?;

        let authorization = WithdrawalAuthorization { auth_hash };
        Signing::sign(&self.signer_key, &authorization.signing_preimage())
    }

    /// Run the proof-client pipeline for the given preimages and return per-block proposals
    /// with an aggregate.
    pub async fn prove(
        &self,
        preimages: impl IntoIterator<Item = (PreimageKey, Vec<u8>)>,
    ) -> Result<ProofResult> {
        let oracle = Oracle::new(preimages)?;

        let boot_info =
            BootInfo::load(&oracle).await.map_err(|e| NitroError::ProofPipeline(e.to_string()))?;
        let config_hash = config_hash(&boot_info.rollup_config)?;
        let agreed_l2_output_root = boot_info.agreed_l2_output_root;

        let prologue = Prologue::new(oracle.clone(), oracle, BaseEvmFactory::default());
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
            let l2_block_number = l2_info.block_info.number;

            let journal = ProofJournal {
                proposer: boot_info.proposer,
                l1_origin_hash,
                prev_output_root,
                starting_l2_block: l2_block_number
                    .checked_sub(1)
                    .ok_or_else(|| NitroError::ProofPipeline("l2_block_number is 0".into()))?,
                output_root: *output_root,
                ending_l2_block: l2_block_number,
                intermediate_roots: vec![],
                config_hash,
                tee_image_hash: self.tee_image_hash,
                schedule_id: boot_info.schedule_id,
            };
            let signing_data = journal.encode();

            let signature = Signing::sign(&self.signer_key, &signing_data)?;

            proposals.push(Proposal {
                output_root: *output_root,
                signature: Bytes::from(signature.to_vec()),
                l1_origin_hash,
                l1_origin_number,
                l2_block_number,
                prev_output_root,
                config_hash,
                schedule_id: boot_info.schedule_id,
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
                    .checked_sub(1)
                    .ok_or_else(|| NitroError::ProofPipeline("l2_block_number is 0".into()))?,
                output_root: last.output_root,
                ending_l2_block: last.l2_block_number,
                intermediate_roots,
                config_hash,
                tee_image_hash: self.tee_image_hash,
                schedule_id: boot_info.schedule_id,
            };
            let signing_data = journal.encode();

            let signature = Signing::sign(&self.signer_key, &signing_data)?;

            Proposal {
                output_root: last.output_root,
                signature: Bytes::from(signature.to_vec()),
                l1_origin_hash,
                l1_origin_number,
                l2_block_number: last.l2_block_number,
                prev_output_root: agreed_l2_output_root,
                config_hash,
                schedule_id: boot_info.schedule_id,
            }
        };

        Ok(ProofResult::Tee { aggregate_proposal, proposals })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;

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
    fn config_hash_missing_system_config() {
        let rollup = RollupConfig {
            genesis: base_common_genesis::ChainGenesis {
                system_config: None,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(config_hash(&rollup).is_err());
    }

    #[test]
    fn config_hash_matches_known_chains() {
        use base_common_chains::ChainConfig;

        for cfg in ChainConfig::all() {
            let rollup = base_common_chains::rollup_config!(cfg);
            let Some(mut per_chain) = PerChainConfig::from_rollup_config(&rollup) else {
                continue;
            };
            per_chain.force_defaults();

            let computed = config_hash(&rollup)
                .unwrap_or_else(|_| panic!("config_hash failed for chain {}", cfg.chain_id));
            assert_eq!(
                per_chain.hash(),
                computed,
                "config hash mismatch for chain {}",
                cfg.chain_id
            );
        }
    }

    #[test]
    fn config_hash_known_values() {
        use base_common_chains::ChainConfig;

        let check = |chain_id: u64, expected: B256| {
            let cfg = ChainConfig::by_chain_id(chain_id)
                .unwrap_or_else(|| panic!("unknown chain {chain_id}"));
            let rollup = cfg.rollup_config();
            assert_eq!(config_hash(&rollup).unwrap(), expected, "chain {chain_id}");
        };

        check(8453, b256!("1607709d90d40904f790574404e2ad614eac858f6162faa0ec34c6bf5e5f3c57"));
        check(84532, b256!("12e9c45f19f9817c6d4385fad29e7a70c355502cf0883e76a9a7e478a85d1360"));
        check(1337, b256!("1bb15c380e7cf5cfd303807cc1dff6cd5275a6facc7628091d8b3a7ab6d631b1"));
        check(763360, b256!("d14ddabfc0ad1dd737d6e5917cf271fd479bd539c9b3d85a602589c679a9983a"));
    }
}
