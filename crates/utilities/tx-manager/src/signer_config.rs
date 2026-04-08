//! Signer configuration for wallet construction.

use std::fmt;

use alloy_network::EthereumWallet;
use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use base_alloy_signer::RemoteSigner;
use url::Url;

use crate::TxManagerError;

/// Describes how to construct an [`EthereumWallet`].
///
/// Used by [`SimpleTxManager::new`](crate::SimpleTxManager::new) to build the
/// wallet internally, centralising wallet construction logic so that call
/// sites do not need to duplicate private-key parsing or remote-signer setup.
pub enum SignerConfig {
    /// Local signer backed by a secp256k1 private key.
    Local(PrivateKeySigner),
    /// Remote signer sidecar via `eth_signTransaction` JSON-RPC.
    Remote {
        /// HTTP endpoint of the remote signer.
        endpoint: Url,
        /// Address of the account managed by the remote signer.
        address: Address,
    },
}

impl fmt::Debug for SignerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(signer) => {
                f.debug_struct("Local").field("address", &signer.address()).finish()
            }
            Self::Remote { endpoint, address } => f
                .debug_struct("Remote")
                .field("endpoint", endpoint)
                .field("address", address)
                .finish(),
        }
    }
}

impl SignerConfig {
    /// Creates a [`Local`](Self::Local) signer config from a [`PrivateKeySigner`].
    pub const fn local(signer: PrivateKeySigner) -> Self {
        Self::Local(signer)
    }

    /// Returns the sender address for this signer configuration.
    pub const fn address(&self) -> Address {
        match self {
            Self::Local(signer) => signer.address(),
            Self::Remote { address, .. } => *address,
        }
    }

    /// Builds an [`EthereumWallet`] from this configuration.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::WalletConstruction`] if the remote signer
    /// client cannot be created.
    pub fn build_wallet(self) -> Result<EthereumWallet, TxManagerError> {
        match self {
            Self::Local(signer) => Ok(EthereumWallet::new(signer)),
            Self::Remote { endpoint, address } => {
                let signer = RemoteSigner::new(endpoint, address)
                    .map_err(|e| TxManagerError::WalletConstruction(e.to_string()))?;
                Ok(EthereumWallet::from(signer))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_valid_key_produces_wallet() {
        let signer = PrivateKeySigner::from_slice(&[0x01; 32]).expect("valid key");
        let config = SignerConfig::local(signer);
        assert!(config.build_wallet().is_ok());
    }

    #[test]
    fn remote_valid_config_produces_wallet() {
        let config = SignerConfig::Remote {
            endpoint: Url::parse("http://127.0.0.1:8080").unwrap(),
            address: Address::repeat_byte(0x42),
        };
        assert!(config.build_wallet().is_ok());
    }
}
