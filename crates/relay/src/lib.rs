//! Encrypted Transaction Relay for Base
//!
//! This crate provides functionality for relaying encrypted transactions to the sequencer
//! via the P2P network, bypassing the traditional mempool.
//!
//! # Overview
//!
//! The encrypted relay system allows users to submit transactions that are encrypted with
//! the sequencer's public key. This provides:
//!
//! - **Privacy**: Transaction contents are hidden from relay nodes
//! - **Resilience**: Alternative path to sequencer when primary RPC is unavailable
//! - **Spam prevention**: Proof-of-work requirement prevents abuse
//!
//! # Components
//!
//! - [`crypto`]: ECIES encryption (X25519 + ChaCha20-Poly1305)
//! - [`pow`]: Proof-of-work computation and verification
//! - [`config`]: On-chain configuration reader
//! - [`types`]: Request/response types for the relay API
//!
//! # Usage
//!
//! ## Encrypting a transaction (client-side)
//!
//! ```ignore
//! use base_reth_relay::{crypto, pow};
//!
//! // Get sequencer's public key from relay parameters
//! let sequencer_pubkey: [u8; 32] = /* from base_getRelayParameters */;
//!
//! // Encrypt the signed transaction
//! let encrypted = crypto::encrypt(&sequencer_pubkey, &signed_tx_bytes)?;
//!
//! // Compute proof-of-work
//! let difficulty = 18; // from relay parameters
//! let nonce = pow::compute(&encrypted, difficulty);
//!
//! // Submit via base_sendEncryptedTransaction RPC
//! ```
//!
//! ## Verifying and forwarding (relay node)
//!
//! ```ignore
//! use base_reth_relay::{pow, config::RelayConfigCache};
//!
//! // Verify the request
//! pow::verify(&request.encrypted_payload, request.pow_nonce, config.pow_difficulty)?;
//!
//! // Check encryption key is valid
//! if !config_cache.is_valid_encryption_key(&request.encryption_pubkey) {
//!     return Err(RelayError::InvalidEncryptionKey);
//! }
//!
//! // Forward to sequencer...
//! ```

#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod attestation;
pub mod config;
pub mod crypto;
pub mod error;
pub mod keys;
pub mod p2p;
pub mod pow;
pub mod types;

// Re-export commonly used items
pub use attestation::{create_attestation, is_attestation_valid, verify_attestation, verify_attestation_full};
pub use config::{RelayConfigCache, RelayConfigReaderConfig, create_config_cache, fetch_config};
pub use crypto::{decrypt, encrypt, generate_keypair};
pub use error::RelayError;
pub use keys::SequencerKeypair;
pub use pow::{DEFAULT_DIFFICULTY, MAX_DIFFICULTY};
pub use types::{
    EncryptedTransactionRequest, EncryptedTransactionResponse, RelayAttestation, RelayErrorCode,
    RelayParameters, MAX_ENCRYPTED_PAYLOAD_SIZE, MIN_ENCRYPTED_PAYLOAD_SIZE,
};
