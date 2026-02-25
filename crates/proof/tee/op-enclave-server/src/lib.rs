//! op-enclave server implementation.
//!
//! This crate provides the server-side enclave logic for AWS Nitro Enclaves,
//! including:
//!
//! - **NSM Interface**: Session management and random number generation using
//!   the Nitro Secure Module
//! - **Attestation**: Verification of AWS Nitro Enclave attestation documents
//! - **Cryptography**: RSA-4096 and ECDSA secp256k1 key operations
//! - **Server**: Main server struct for key management and attestation
//! - **RPC**: JSON-RPC server interface using jsonrpsee
//! - **Transport**: Vsock and HTTP transport configuration
//!
//! # Local Mode
//!
//! When running outside of a Nitro Enclave (or on non-Linux platforms),
//! the server operates in "local mode":
//! - No NSM operations (attestation unavailable)
//! - Uses OS random number generator
//! - Optionally loads signer key from `OP_ENCLAVE_SIGNER_KEY` environment variable
//!
//! # Example
//!
//! ```no_run
//! use op_enclave_server::Server;
//!
//! let server = Server::new().expect("failed to create server");
//! println!("Signer address: {}", server.signer_address());
//! println!("Local mode: {}", server.is_local_mode());
//! ```

#![warn(missing_docs)]
#![warn(unreachable_pub)]
#![deny(unused_must_use)]
#![deny(rust_2018_idioms)]

pub mod attestation;
pub mod crypto;
pub mod error;
pub mod nsm;
pub mod rpc;
mod server;
pub mod transport;

// Re-export core types
// Re-export commonly used types
pub use attestation::{
    AttestationDocument, VerificationResult, verify_attestation, verify_attestation_with_pcr0,
};
pub use crypto::{
    SIGNATURE_LENGTH, SIGNING_DATA_BASE_LENGTH, build_signing_data, decrypt_pkcs1v15,
    encrypt_pkcs1v15, generate_rsa_key, generate_signer, pkix_to_public_key, private_key_bytes,
    public_key_bytes, public_key_to_pkix, sign_proposal_data_sync, signer_from_bytes,
    signer_from_hex, verify_proposal_signature,
};
pub use error::{AttestationError, CryptoError, NsmError, ProposalError, Result, ServerError};
pub use nsm::{NsmRng, NsmSession};
pub use op_enclave_core::{
    AccountResult, Address, AggregateRequest, B256, BlockId, BlockInfoWrapper, BlockNumHash, Bytes,
    ChainConfig, ChainGenesis, ConfigError, DEPOSIT_EVENT_TOPIC, EnclaveError, EnclaveTrieDB,
    ExecuteStatelessRequest, ExecutionResult, ExecutionWitness, ExecutorError, Genesis,
    GenesisSystemConfig, HardForkConfig, Header, L1_ATTRIBUTES_DEPOSITOR,
    L1_ATTRIBUTES_PREDEPLOYED, L1ChainConfig, L1ReceiptsFetcher, L2_TO_L1_MESSAGE_PASSER,
    L2SystemConfigFetcher, MARSHAL_BINARY_SIZE, MAX_SEQUENCER_DRIFT_FJORD, OpReceiptEnvelope,
    PerChainConfig, Proposal, ProposalParams, ProviderError, RollupConfig, SystemConfig,
    TransformedWitness, U256, compute_receipt_root, compute_tx_root, execute_stateless,
    extract_deposits_from_receipts, l2_block_to_block_info, output_root_v0, transform_witness,
    validate_not_deposit, validate_sequencer_drift,
};
// Re-export server
pub use server::Server;
