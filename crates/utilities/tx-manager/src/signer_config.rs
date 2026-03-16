//! Signer configuration for wallet construction.

use std::fmt;

use alloy_network::EthereumWallet;
use alloy_primitives::{Address, B256};
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
    /// Local signer backed by a raw secp256k1 private key.
    Local {
        /// The 32-byte private key.
        private_key: B256,
    },
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
            Self::Local { .. } => {
                f.debug_struct("Local").field("private_key", &"[REDACTED]").finish()
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
    /// Builds an [`EthereumWallet`] from this configuration.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::WalletConstruction`] if the private key is
    /// invalid or the remote signer client cannot be created.
    pub fn build_wallet(self) -> Result<EthereumWallet, TxManagerError> {
        match self {
            Self::Local { private_key } => {
                let signer = PrivateKeySigner::from_bytes(&private_key)
                    .map_err(|e| TxManagerError::WalletConstruction(e.to_string()))?;
                Ok(EthereumWallet::new(signer))
            }
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
        // Use a well-known non-zero private key.
        let key = B256::repeat_byte(0x01);
        let config = SignerConfig::Local { private_key: key };
        assert!(config.build_wallet().is_ok());
    }

    #[test]
    fn local_zero_key_returns_error() {
        let config = SignerConfig::Local { private_key: B256::ZERO };
        let err = config.build_wallet().expect_err("zero key should fail");
        assert!(
            matches!(err, TxManagerError::WalletConstruction(_)),
            "expected WalletConstruction, got {err:?}",
        );
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
