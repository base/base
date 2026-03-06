#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod error;
pub use error::{RangeProverError, TeeProverError};

mod traits;
pub use traits::{ExecutionWitnessProvider, TeeExecutor};

mod transaction;
pub use transaction::{DEPOSIT_TX_TYPE, TransactionSerializer};

mod receipt;
pub use receipt::ReceiptConverter;

mod config;
pub use config::ConfigBuilder;

mod proof;
pub use proof::{
    ECDSA_SIGNATURE_LENGTH, ECDSA_V_OFFSET, ENCLAVE_TIMEOUT, PROOF_TYPE_TEE, ProofEncoder,
};

mod range_prover;
pub use range_prover::RangeProver;
