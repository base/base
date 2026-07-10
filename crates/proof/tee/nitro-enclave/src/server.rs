/// Enclave server — manages keys, attestation, signing, and proof execution.
use std::sync::LazyLock;

use alloy_primitives::{Address, B256, keccak256, map::HashMap};
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_evm::BaseEvmFactory;
use base_common_genesis::RollupConfig;
use base_proof_client::{Prologue, TeeProposals};
use base_proof_preimage::PreimageKey;
use base_proof_primitives::{PerChainConfig, ProofResult};
use tracing::info;

use crate::{
    Ecdsa, NsmRng, NsmSession, Oracle, Signing,
    error::{NitroError, NsmError, ProposalError, Result},
};

/// Environment variable for setting the signer key in local mode.
const SIGNER_KEY_ENV_VAR: &str = "BASE_ENCLAVE_SIGNER_KEY";

/// PCR0 is a SHA-384 hash (48 bytes) per the AWS Nitro Enclaves specification.
const PCR0_LENGTH: usize = 48;

/// Per-chain config hashes derived from [`ChainConfig::all`] at first access.
///
/// Each entry is `keccak256(PerChainConfig::marshal_binary())` with defaults applied.
/// Chains that lack a `system_config` in their rollup config are skipped.
static CONFIG_HASHES: LazyLock<HashMap<u64, B256>> = LazyLock::new(|| {
    let mut map = HashMap::default();
    for cfg in ChainConfig::all() {
        let rollup = RollupConfig::from(cfg);
        if let Some(mut per_chain) = PerChainConfig::from_rollup_config(&rollup) {
            per_chain.force_defaults();
            map.insert(cfg.chain_id, per_chain.hash());
        }
    }
    map
});

/// Look up the config hash for a supported chain.
fn config_hash_for_chain(chain_id: u64) -> Result<B256> {
    CONFIG_HASHES.get(&chain_id).copied().ok_or(NitroError::UnsupportedChain(chain_id))
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

    /// Run the proof-client pipeline for the given preimages and return per-block proposals
    /// with an aggregate.
    pub async fn prove(
        &self,
        preimages: impl IntoIterator<Item = (PreimageKey, Vec<u8>)>,
    ) -> Result<ProofResult> {
        let oracle = Oracle::new(preimages)?;
        let prologue = Prologue::new(oracle.clone(), oracle, BaseEvmFactory::default());
        let (boot_info, driver) =
            prologue.load().await.map_err(|e| NitroError::ProofPipeline(e.to_string()))?;
        let config_hash = config_hash_for_chain(boot_info.chain_id)?;
        let (epilogue, block_results) = driver
            .execute_with_intermediates()
            .await
            .map_err(|e| NitroError::ProofPipeline(e.to_string()))?;

        // Trust-critical: validate final output root against claim
        epilogue.validate().map_err(|e| NitroError::ProofPipeline(e.to_string()))?;

        TeeProposals::build(
            &boot_info,
            &block_results,
            config_hash,
            self.tee_image_hash,
            |data| Signing::sign(&self.signer_key, data),
            |message| match message {
                TeeProposals::EMPTY_PROPOSALS_ERROR => ProposalError::EmptyProposals.into(),
                TeeProposals::INTERMEDIATE_BLOCK_INTERVAL_ZERO_ERROR => {
                    ProposalError::InvalidInterval.into()
                }
                _ => NitroError::ProofPipeline(message.into()),
            },
        )
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
    fn config_hash_unknown_chain() {
        assert!(config_hash_for_chain(999999).is_err());
    }

    #[test]
    fn config_hashes_match_chain_configs() {
        for cfg in ChainConfig::all() {
            let chain_id = cfg.chain_id;
            let rollup = base_common_chains::rollup_config!(cfg);
            let Some(mut per_chain) = PerChainConfig::from_rollup_config(&rollup) else {
                continue;
            };
            per_chain.force_defaults();

            let cached = config_hash_for_chain(chain_id)
                .unwrap_or_else(|_| panic!("missing config hash for chain {chain_id}"));
            assert_eq!(per_chain.hash(), cached, "config hash mismatch for chain {chain_id}");
        }
    }

    /// Print config hashes for supported chains so they can be hardcoded in the
    /// enclave server. Run with:
    /// `cargo test -p base-proof-tee-nitro-enclave print_real_config_hashes -- --nocapture --ignored`
    #[test]
    #[ignore]
    fn print_real_config_hashes() {
        for cfg in ChainConfig::all() {
            let chain_id = cfg.chain_id;
            let rollup = base_common_chains::rollup_config!(cfg);
            let mut per_chain = match PerChainConfig::from_rollup_config(&rollup) {
                Some(pc) => pc,
                None => {
                    println!("chain {chain_id}: skipped (no system_config)");
                    continue;
                }
            };
            per_chain.force_defaults();
            println!("chain {chain_id}: {:?}", per_chain.hash());
        }
    }

    #[test]
    fn config_hash_known_values() {
        assert_eq!(
            config_hash_for_chain(8453).unwrap(),
            b256!("1607709d90d40904f790574404e2ad614eac858f6162faa0ec34c6bf5e5f3c57"),
        );
        assert_eq!(
            config_hash_for_chain(84532).unwrap(),
            b256!("12e9c45f19f9817c6d4385fad29e7a70c355502cf0883e76a9a7e478a85d1360"),
        );
        assert_eq!(
            config_hash_for_chain(1337).unwrap(),
            b256!("1bb15c380e7cf5cfd303807cc1dff6cd5275a6facc7628091d8b3a7ab6d631b1"),
        );
        assert_eq!(
            config_hash_for_chain(763360).unwrap(),
            b256!("d14ddabfc0ad1dd737d6e5917cf271fd479bd539c9b3d85a602589c679a9983a"),
        );
    }
}
