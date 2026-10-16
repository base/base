#![doc = include_str!("../README.md")]

mod config;
pub use config::{ConfigError, Fees, SignerSource, WithdrawerArgs, WithdrawerConfig};

mod network;
pub use network::{Network, NetworkPreset, ProofSystem};

mod rpc;
pub use rpc::{BroadcastResult, WithdrawalRpc, WithdrawalRpcError};

mod transport;
pub use transport::WithdrawalRpcTransport;

mod signer;
pub use signer::{WithdrawalSigner, WithdrawalSignerError};

mod secret;
pub use secret::{Secret, SecretValueParser};
