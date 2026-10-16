#![doc = include_str!("../README.md")]

mod contracts;
pub use contracts::{
    FaultProofPortal, LegacyWithdrawalPortal, WithdrawalDisputeGame, WithdrawalOutputOracle,
};

mod config;
pub use config::{
    ConfigError, Fees, RpcUrlValueParser, SignerSource, WithdrawerArgs, WithdrawerConfig,
};

mod network;
pub use network::{Network, NetworkPreset, ProofSystem};

mod rpc;
pub use rpc::{BroadcastResult, WithdrawalRpc, WithdrawalRpcError};

mod transport;
pub use transport::WithdrawalRpcTransport;

mod signer;
pub use signer::{WithdrawalSigner, WithdrawalSignerError};

mod transaction;
pub use transaction::{PreparedWithdrawalTransaction, WithdrawalTransactionError};

mod secret;
pub use secret::{Secret, SecretValueParser};

mod withdrawal;
pub use withdrawal::{
    MessagePassed, OutputRootProof, VerifiedWithdrawalProof, Withdrawal, WithdrawalError,
    WithdrawalPortal, WithdrawalTransaction,
};
