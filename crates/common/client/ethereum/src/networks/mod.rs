//! Ethereum and Base networks, transaction builders, wallets, and signers.

mod error;
pub use error::RemoteSignerError;

mod signer;
pub use signer::RemoteSigner;

mod traits;
pub use traits::EthSignerApiClient;

mod local;
pub use local::PrivateKeySigner;

mod local_error;
pub use local_error::LocalSignerError;

mod private_key;

#[cfg(feature = "mnemonic")]
mod mnemonic;
#[cfg(feature = "mnemonic")]
pub use coins_bip39;
#[cfg(feature = "mnemonic")]
pub use mnemonic::{MnemonicBuilder, MnemonicBuilderError, MnemonicKey, MnemonicSignerIter};

mod transaction;
pub use transaction::{
    BuildResult, FullSigner, FullSignerSync, NetworkTransactionBuilder, NetworkWallet,
    TransactionBuilder, TransactionBuilder4844, TransactionBuilder7702, TransactionBuilderError,
    TxSigner, TxSignerSync, UnbuiltTransactionError,
};

mod ethereum;
/// Types for handling unknown network types.
pub use alloy_eips::eip2718;
pub use base_common_types_rpc::{
    self as primitives, BlockResponse, ReceiptResponse, TransactionResponse,
};
pub use ethereum::{Ethereum, EthereumWallet, IntoWallet};

mod network;
pub use network::Network;

mod wallet_macro;

mod base;
pub use base::Base;

mod base_builder;
