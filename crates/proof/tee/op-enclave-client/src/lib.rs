//! op-enclave client implementation.
//!
//! This crate provides a client for communicating with the enclave RPC server.
//!
//! # Example
//!
//! ```ignore
//! use op_enclave_client::EnclaveClient;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = EnclaveClient::new("http://127.0.0.1:1234")?;
//!
//!     let public_key = client.signer_public_key().await?;
//!     println!("Signer public key: {:?}", public_key);
//!
//!     Ok(())
//! }
//! ```

#![warn(missing_docs)]
#![warn(unreachable_pub)]
#![deny(unused_must_use)]
#![deny(rust_2018_idioms)]

mod client;
mod client_error;

pub use client::EnclaveClient;
pub use client_error::ClientError;
// Re-export core types
pub use op_enclave_core::{
    AccountResult, Address, AggregateRequest, B256, BlockId, BlockInfoWrapper, BlockNumHash, Bytes,
    ChainConfig, ChainGenesis, ConfigError, CryptoError, DEPOSIT_EVENT_TOPIC, EnclaveError,
    EnclaveTrieDB, ExecuteStatelessRequest, ExecutionResult, ExecutionWitness, ExecutorError,
    Genesis, GenesisSystemConfig, HardForkConfig, Header, L1_ATTRIBUTES_DEPOSITOR,
    L1_ATTRIBUTES_PREDEPLOYED, L1ChainConfig, L1ReceiptsFetcher, L2_TO_L1_MESSAGE_PASSER,
    L2SystemConfigFetcher, MARSHAL_BINARY_SIZE, MAX_SEQUENCER_DRIFT_FJORD, OpReceiptEnvelope,
    PerChainConfig, Proposal, ProposalParams, ProviderError, Result, RollupConfig, SystemConfig,
    TransformedWitness, U256, compute_receipt_root, compute_tx_root, execute_stateless,
    extract_deposits_from_receipts, l2_block_to_block_info, output_root_v0, transform_witness,
    validate_not_deposit, validate_sequencer_drift,
};
