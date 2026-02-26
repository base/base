//! Enclave server implementation.
//!
//! This module contains the main [`Server`] struct that manages cryptographic
//! keys and attestation for the enclave.

use alloy_consensus::{Header, ReceiptEnvelope};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_signer_local::PrivateKeySigner;
use base_consensus_genesis::{L1ChainConfig, RollupConfig};
use base_enclave::{
    Proposal, ProposalParams,
    executor::{ExecutionWitness, execute_stateless as core_execute_stateless},
    output_root_v0,
    types::account::AccountResult,
};
use parking_lot::RwLock;
#[cfg(test)]
use rand_08::rngs::OsRng;
use rsa::RsaPrivateKey;
use tracing::{info, warn};

use crate::{
    attestation::{extract_public_key, verify_attestation_with_pcr0},
    crypto::{
        build_signing_data, decrypt_pkcs1v15, encrypt_pkcs1v15, generate_rsa_key, generate_signer,
        pkix_to_public_key, private_key_bytes, private_to_public, public_key_bytes,
        public_key_to_pkix, sign_proposal_data_sync, signer_from_bytes, signer_from_hex,
        verify_proposal_signature,
    },
    error::{CryptoError, NsmError, ProposalError, ServerError},
    nsm::{NsmRng, NsmSession},
};

/// Environment variable for setting the signer key in local mode.
const SIGNER_KEY_ENV_VAR: &str = "OP_ENCLAVE_SIGNER_KEY";

/// The enclave server.
///
/// Manages cryptographic keys and attestation for the enclave.
/// Supports both Nitro Enclave mode (with NSM) and local mode (for development).
#[derive(Debug)]
pub struct Server {
    /// PCR0 measurement (empty in local mode).
    pcr0: Vec<u8>,
    /// ECDSA signing key with interior mutability for `set_signer_key`.
    signer_key: RwLock<PrivateKeySigner>,
    /// RSA-4096 key for decrypting received signer keys.
    decryption_key: RsaPrivateKey,
}

impl Server {
    /// Create a new server instance.
    ///
    /// This attempts to open an NSM session. If successful, it reads PCR0
    /// and uses NSM for random number generation. If NSM is unavailable,
    /// it falls back to local mode and optionally reads the signer key
    /// from the `OP_ENCLAVE_SIGNER_KEY` environment variable.
    pub fn new() -> Result<Self, ServerError> {
        let (mut rng, pcr0, signer_key_env) = match NsmSession::open()? {
            Some(session) => {
                let pcr0 = session.describe_pcr0()?;
                let rng = NsmRng::new().ok_or_else(|| {
                    NsmError::SessionOpen("failed to initialize NSM RNG".to_string())
                })?;
                (rng, pcr0, None)
            }
            None => {
                warn!("running in local mode without NSM");
                let rng = NsmRng::default();
                let signer_key_env = std::env::var(SIGNER_KEY_ENV_VAR).ok();
                (rng, Vec::new(), signer_key_env)
            }
        };

        // Generate RSA decryption key
        let decryption_key = generate_rsa_key(&mut rng)?;

        // Generate or load ECDSA signer key
        let signer_key = if let Some(hex_key) = signer_key_env {
            info!("using signer key from environment variable");
            signer_from_hex(&hex_key)?
        } else {
            generate_signer(&mut rng)?
        };

        let local_mode = pcr0.is_empty();
        info!(
            address = %signer_key.address(),
            local_mode = local_mode,
            "server initialized"
        );

        Ok(Self { pcr0, signer_key: RwLock::new(signer_key), decryption_key })
    }

    /// Check if the server is running in local mode.
    #[must_use]
    pub const fn is_local_mode(&self) -> bool {
        self.pcr0.is_empty()
    }

    /// Get the PCR0 measurement.
    ///
    /// Returns an empty vector in local mode.
    #[must_use]
    pub fn pcr0(&self) -> &[u8] {
        &self.pcr0
    }

    /// Get the signer's public key as a 65-byte uncompressed EC point.
    ///
    /// This matches Go's `crypto.FromECDSAPub()` format.
    #[must_use]
    pub fn signer_public_key(&self) -> Vec<u8> {
        let signer = self.signer_key.read();
        public_key_bytes(&signer)
    }

    /// Get the signer's Ethereum address.
    #[must_use]
    pub fn signer_address(&self) -> Address {
        let signer = self.signer_key.read();
        signer.address()
    }

    /// Get an attestation document containing the signer's public key.
    ///
    /// Returns an error in local mode.
    pub fn signer_attestation(&self) -> Result<Vec<u8>, ServerError> {
        self.public_key_attestation(|| Ok(self.signer_public_key()))
    }

    /// Get the decryption public key in PKIX/DER format.
    ///
    /// This matches Go's `x509.MarshalPKIXPublicKey()` format.
    pub fn decryption_public_key(&self) -> Result<Vec<u8>, ServerError> {
        let public_key = private_to_public(&self.decryption_key);
        public_key_to_pkix(&public_key)
    }

    /// Get an attestation document containing the decryption public key.
    ///
    /// Returns an error in local mode.
    pub fn decryption_attestation(&self) -> Result<Vec<u8>, ServerError> {
        self.public_key_attestation(|| self.decryption_public_key())
    }

    /// Get an attestation document containing the given public key.
    fn public_key_attestation<F>(&self, public_key_fn: F) -> Result<Vec<u8>, ServerError>
    where
        F: FnOnce() -> Result<Vec<u8>, ServerError>,
    {
        let session = NsmSession::open()?
            .ok_or_else(|| NsmError::SessionOpen("NSM not available".to_string()))?;

        let public_key = public_key_fn()?;
        session.get_attestation(public_key)
    }

    /// Encrypt the signer key for a remote enclave.
    ///
    /// This verifies the provided attestation document, checks that PCR0 matches,
    /// and encrypts the signer key using the public key from the attestation.
    ///
    /// # Arguments
    /// * `attestation` - The attestation document from the remote enclave
    ///
    /// # Returns
    /// The encrypted signer key (can be decrypted with `set_signer_key`)
    pub fn encrypted_signer_key(&self, attestation: &[u8]) -> Result<Vec<u8>, ServerError> {
        // Verify attestation and check PCR0
        let result = verify_attestation_with_pcr0(attestation, &self.pcr0)?;

        // Extract and parse the public key
        let public_key_der = extract_public_key(&result.document)?;
        let public_key = pkix_to_public_key(&public_key_der)?;

        // Get the signer's private key
        let signer = self.signer_key.read();
        let signer_private_key = private_key_bytes(&signer);

        // Encrypt with NSM RNG
        let mut rng = NsmRng::default();
        encrypt_pkcs1v15(&mut rng, &public_key, &signer_private_key)
    }

    /// Set the signer key from an encrypted key.
    ///
    /// This decrypts the provided ciphertext using the decryption key
    /// and updates the signer key.
    ///
    /// # Arguments
    /// * `encrypted` - The encrypted signer key (from `encrypted_signer_key`)
    pub fn set_signer_key(&self, encrypted: &[u8]) -> Result<(), ServerError> {
        let decrypted = decrypt_pkcs1v15(&self.decryption_key, encrypted)?;

        if decrypted.len() != 32 {
            return Err(CryptoError::InvalidPrivateKeyLength(decrypted.len()).into());
        }

        let new_signer = signer_from_bytes(&decrypted)?;

        info!(
            address = %new_signer.address(),
            "updated signer key"
        );

        *self.signer_key.write() = new_signer;
        Ok(())
    }

    /// Get a read guard for the signer.
    ///
    /// Use this for signing operations.
    pub fn signer(&self) -> parking_lot::RwLockReadGuard<'_, PrivateKeySigner> {
        self.signer_key.read()
    }

    /// Execute stateless block validation and create a signed proposal.
    ///
    /// This method:
    /// 1. Extracts the previous block header from the witness
    /// 2. Calls the core `execute_stateless` function to validate the block
    /// 3. Computes the previous and current output roots
    /// 4. Signs the proposal data and returns a `Proposal`
    ///
    /// # Arguments
    ///
    /// * `rollup_config` - The rollup configuration
    /// * `l1_config` - The L1 chain configuration
    /// * `config_hash` - The per-chain configuration hash
    /// * `l1_origin` - The L1 origin block header
    /// * `l1_receipts` - The L1 origin block receipts
    /// * `previous_block_txs` - Transactions from the previous L2 block (RLP-encoded)
    /// * `block_header` - The L2 block header to validate
    /// * `sequenced_txs` - Sequenced transactions for this block (RLP-encoded)
    /// * `witness` - The execution witness (contains previous block header at headers[0])
    /// * `message_account` - The `L2ToL1MessagePasser` account proof
    /// * `prev_message_account_hash` - The storage hash of the message account in the previous block
    ///
    /// # Returns
    ///
    /// A signed `Proposal` containing the output root and signature.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_stateless(
        &self,
        rollup_config: &RollupConfig,
        l1_config: &L1ChainConfig,
        config_hash: B256,
        l1_origin: &Header,
        l1_receipts: &[ReceiptEnvelope],
        previous_block_txs: &[Bytes],
        block_header: &Header,
        sequenced_txs: &[Bytes],
        witness: ExecutionWitness,
        message_account: &AccountResult,
        prev_message_account_hash: B256,
        proposer: Address,
        tee_image_hash: B256,
    ) -> Result<Proposal, ServerError> {
        // Extract previous block header from witness before consuming it
        // (matches Go: previousBlockHeader := w.Headers[0])
        let previous_header = witness.headers.first().cloned().ok_or_else(|| {
            ProposalError::ExecutionFailed("witness headers is empty".to_string())
        })?;

        // Execute stateless validation using the core function
        core_execute_stateless(
            rollup_config,
            l1_config,
            l1_origin,
            l1_receipts,
            previous_block_txs,
            block_header,
            sequenced_txs,
            witness,
            message_account,
        )
        .map_err(|e| ProposalError::ExecutionFailed(e.to_string()))?;

        let l1_origin_hash = l1_origin.hash_slow();
        let l1_origin_number = U256::from(l1_origin.number);

        // Compute output roots (matching Go implementation exactly)
        let prev_output_root = output_root_v0(&previous_header, prev_message_account_hash);
        let output_root = output_root_v0(block_header, message_account.storage_hash);
        let starting_l2_block = U256::from(previous_header.number);
        let ending_l2_block = U256::from(block_header.number);

        // Build signing data matching the AggregateVerifier contract's journal.
        // Individual block proofs have no intermediate roots.
        let signing_data = build_signing_data(
            proposer,
            l1_origin_hash,
            l1_origin_number,
            prev_output_root,
            starting_l2_block,
            output_root,
            ending_l2_block,
            &[],
            config_hash,
            tee_image_hash,
        );

        // Sign the proposal
        let signer = self.signer_key.read();
        let signature = sign_proposal_data_sync(&signer, &signing_data)?;

        Ok(Proposal::new(ProposalParams {
            output_root,
            signature,
            l1_origin_hash,
            l1_origin_number,
            l2_block_number: ending_l2_block,
            prev_output_root,
            config_hash,
        }))
    }

    /// Create a server for testing without RSA key generation.
    ///
    /// This is much faster than `new()` because RSA-4096 key generation takes ~7s
    /// in debug mode. Tests that don't need RSA decryption should use this.
    ///
    /// # Warning
    /// The decryption key is NOT usable - do not use for actual encryption/decryption tests.
    #[cfg(test)]
    pub fn new_for_testing() -> Result<Self, ServerError> {
        let mut rng = OsRng;

        // Generate ECDSA signer (fast, ~1ms)
        let signer_key = generate_signer(&mut rng)?;

        info!(
            address = %signer_key.address(),
            "test server initialized (no RSA key)"
        );

        // Use a minimal RSA key (smallest allowed: 64 bytes = 512 bits) for struct completeness.
        // This key is NOT secure and MUST NOT be used for actual encryption.
        let decryption_key = rsa::RsaPrivateKey::new(&mut rng, 512)
            .map_err(|e| CryptoError::RsaKeyGeneration(e.to_string()))?;

        Ok(Self { pcr0: Vec::new(), signer_key: RwLock::new(signer_key), decryption_key })
    }

    /// Aggregate multiple proposals into a single proposal.
    ///
    /// This method:
    /// 1. Verifies the signature chain of all proposals
    /// 2. Creates a new aggregated proposal spanning from the first to last block
    ///
    /// # Arguments
    ///
    /// * `config_hash` - The per-chain configuration hash
    /// * `prev_output_root` - The output root before the first proposal
    /// * `proposals` - The proposals to aggregate
    ///
    /// # Returns
    ///
    /// A new signed proposal covering the entire range.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No proposals are provided
    /// - Any signature verification fails
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
    ) -> Result<Proposal, ServerError> {
        if proposals.is_empty() {
            return Err(ProposalError::EmptyProposals.into());
        }

        // Single proposal: return as-is without re-signing. Intermediate roots
        // only apply to multi-block aggregated proposals (BLOCK_INTERVAL > 1), so
        // they must be empty here. This path is used for sub-aggregation batches
        // and would also be hit if BLOCK_INTERVAL == INTERMEDIATE_BLOCK_INTERVAL.
        if proposals.len() == 1 {
            if !intermediate_roots.is_empty() {
                return Err(ProposalError::ExecutionFailed(
                    "intermediate roots must be empty for single-proposal aggregation".to_string(),
                )
                .into());
            }
            return Ok(proposals[0].clone());
        }

        // Get our public key for signature verification
        let public_key = self.signer_public_key();

        // Verify the signature chain
        let mut output_root = prev_output_root;
        let mut l1_origin_hash = B256::ZERO;
        let mut l1_origin_number = U256::ZERO;
        let mut l2_block_number = U256::ZERO;
        let mut prev_l2_block = U256::from(prev_block_number);

        for (index, proposal) in proposals.iter().enumerate() {
            l1_origin_hash = proposal.l1_origin_hash;
            l1_origin_number = proposal.l1_origin_number;
            l2_block_number = proposal.l2_block_number;

            // Input proposals were signed without intermediate roots.
            let signing_data = build_signing_data(
                proposer,
                l1_origin_hash,
                l1_origin_number,
                output_root,
                prev_l2_block,
                proposal.output_root,
                l2_block_number,
                &[],
                config_hash,
                tee_image_hash,
            );

            let valid = verify_proposal_signature(&public_key, &signing_data, &proposal.signature)
                .map_err(|_| ProposalError::InvalidSignature { index })?;

            if !valid {
                return Err(ProposalError::InvalidSignature { index }.into());
            }

            output_root = proposal.output_root;
            prev_l2_block = l2_block_number;
        }

        // Validate intermediate roots against the individual proposals' output roots.
        // Each intermediate root must match the output_root of the proposal at the
        // corresponding block boundary, preventing a compromised proposer from
        // submitting fake intermediate checkpoints.
        if !intermediate_roots.is_empty() && proposals.len() > 1 {
            let total_blocks =
                proposals.last().unwrap().l2_block_number.to::<u64>() - prev_block_number;
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

        // Create the aggregated proposal with intermediate roots in the journal.
        let final_output_root = output_root;
        let starting_l2_block = U256::from(prev_block_number);

        let signing_data = build_signing_data(
            proposer,
            l1_origin_hash,
            l1_origin_number,
            prev_output_root,
            starting_l2_block,
            final_output_root,
            l2_block_number,
            intermediate_roots,
            config_hash,
            tee_image_hash,
        );

        let signer = self.signer_key.read();
        let signature = sign_proposal_data_sync(&signer, &signing_data)?;

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
}

#[cfg(test)]
mod tests {
    use std::slice;

    use super::*;

    #[test]
    fn test_server_new_local_mode() {
        // On non-Linux or without NSM, should create in local mode
        let server = Server::new().expect("failed to create server");

        #[cfg(not(target_os = "linux"))]
        assert!(server.is_local_mode());

        // Should have a valid signer key
        let public_key = server.signer_public_key();
        assert_eq!(public_key.len(), 65);
        assert_eq!(public_key[0], 0x04);
    }

    #[test]
    fn test_decryption_public_key() {
        let server = Server::new().expect("failed to create server");
        let public_key = server.decryption_public_key().expect("failed to get key");
        // PKIX-encoded RSA-4096 public key should be around 550 bytes
        assert!(public_key.len() > 500);
    }

    #[test]
    #[ignore = "slow: generates two RSA-4096 keys"]
    fn test_key_exchange_between_servers() {
        // Simulate key exchange between two local servers
        let server1 = Server::new().expect("failed to create server1");
        let server2 = Server::new().expect("failed to create server2");

        // In local mode, we can't use attestation, but we can test the RSA path
        let signer = server1.signer();
        let private_key = private_key_bytes(&signer);
        drop(signer);

        // Encrypt the key with server2's decryption key
        let public_key = server2.decryption_public_key().expect("failed to get key");
        let parsed_key = pkix_to_public_key(&public_key).expect("failed to parse key");

        let mut rng = rand_08::rngs::OsRng;
        let encrypted =
            encrypt_pkcs1v15(&mut rng, &parsed_key, &private_key).expect("failed to encrypt");

        // Decrypt and set on server2
        server2.set_signer_key(&encrypted).expect("failed to set key");

        // Verify both servers now have the same signer address
        assert_eq!(server1.signer_address(), server2.signer_address());
    }

    #[test]
    fn test_signer_address_consistency() {
        let server = Server::new().expect("failed to create server");

        // Address should be consistent across calls
        let addr1 = server.signer_address();
        let addr2 = server.signer_address();
        assert_eq!(addr1, addr2);

        // Public key should also be consistent
        let pk1 = server.signer_public_key();
        let pk2 = server.signer_public_key();
        assert_eq!(pk1, pk2);
    }

    #[test]
    fn test_aggregate_empty_proposals() {
        let server = Server::new_for_testing().expect("failed to create server");

        let result =
            server.aggregate(B256::ZERO, B256::ZERO, 99u64, &[], Address::ZERO, B256::ZERO, &[]);

        assert!(matches!(result, Err(ServerError::Proposal(ProposalError::EmptyProposals))));
    }

    #[test]
    fn test_aggregate_single_proposal() {
        let server = Server::new_for_testing().expect("failed to create server");

        let config_hash = B256::repeat_byte(0x11);
        let l1_origin_hash = B256::repeat_byte(0x22);
        let l1_origin_number = U256::from(100);
        let l2_block_number = U256::from(100);
        let prev_output_root = B256::repeat_byte(0x33);
        let output_root = B256::repeat_byte(0x44);
        let proposer = Address::ZERO;
        let tee_image_hash = B256::ZERO;
        let prev_block_number = 99u64;

        let signing_data = build_signing_data(
            proposer,
            l1_origin_hash,
            l1_origin_number,
            prev_output_root,
            U256::from(prev_block_number),
            output_root,
            l2_block_number,
            &[],
            config_hash,
            tee_image_hash,
        );

        let signer = server.signer();
        let signature = sign_proposal_data_sync(&signer, &signing_data).expect("signing failed");
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
        let server = Server::new_for_testing().expect("failed to create server");

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

        // Create first proposal (block 100, previous block 99)
        let signing_data_1 = build_signing_data(
            proposer,
            l1_origin_hash_1,
            l1_origin_number,
            prev_output_root,
            U256::from(prev_block_number),
            output_root_1,
            U256::from(100),
            &[],
            config_hash,
            tee_image_hash,
        );

        let signer = server.signer();
        let signature_1 =
            sign_proposal_data_sync(&signer, &signing_data_1).expect("signing failed");
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

        // Create second proposal (block 101, chained from first)
        let signing_data_2 = build_signing_data(
            proposer,
            l1_origin_hash_2,
            l1_origin_number,
            output_root_1,
            U256::from(100),
            output_root_2,
            U256::from(101),
            &[],
            config_hash,
            tee_image_hash,
        );

        let signer = server.signer();
        let signature_2 =
            sign_proposal_data_sync(&signer, &signing_data_2).expect("signing failed");
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
        let server = Server::new_for_testing().expect("failed to create server");

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

        // Create a valid first proposal
        let signing_data_1 = build_signing_data(
            proposer,
            l1_origin_hash_1,
            l1_origin_number,
            prev_output_root,
            U256::from(prev_block_number),
            output_root_1,
            U256::from(100),
            &[],
            config_hash,
            tee_image_hash,
        );

        let signer = server.signer();
        let signature_1 =
            sign_proposal_data_sync(&signer, &signing_data_1).expect("signing failed");
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

        // Create a second proposal with an invalid signature (random bytes)
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

        // The second proposal (index 1) should fail signature verification
        assert!(
            matches!(
                &result,
                Err(ServerError::Proposal(ProposalError::InvalidSignature { index: 1 }))
            ),
            "Expected InvalidSignature error at index 1, got: {result:?}"
        );
    }

    /// Helper: create a signed proposal for a single block.
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
        let signing_data = build_signing_data(
            proposer,
            l1_origin_hash,
            l1_origin_number,
            prev_output_root,
            U256::from(prev_block),
            output_root,
            U256::from(block),
            &[],
            config_hash,
            tee_image_hash,
        );
        let signer = server.signer();
        let signature = sign_proposal_data_sync(&signer, &signing_data).expect("signing failed");
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
        let server = Server::new_for_testing().expect("failed to create server");

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

        // 3 blocks, 1 intermediate root => interval = 3, target block = 3
        // proposals[2].output_root == output_root_3
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
        let server = Server::new_for_testing().expect("failed to create server");

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
                Err(ServerError::Proposal(ProposalError::InvalidIntermediateRoot { .. }))
            ),
            "Expected InvalidIntermediateRoot error, got: {result:?}"
        );
    }

    #[test]
    fn test_aggregate_rejects_intermediate_roots_with_single_proposal() {
        let server = Server::new_for_testing().expect("failed to create server");

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
            matches!(&result, Err(ServerError::Proposal(ProposalError::ExecutionFailed(_)))),
            "Expected ExecutionFailed error for single proposal with intermediate roots, got: {result:?}"
        );
    }
}
