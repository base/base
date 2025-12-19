//! On-chain configuration reader for encrypted relay.
//!
//! Reads relay parameters from the `EncryptedRelayConfig` contract and caches them.

use std::sync::Arc;

use alloy_primitives::{Address, Bytes, B256};
use alloy_provider::Provider;
use alloy_sol_macro::sol;
use arc_swap::ArcSwap;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::{
    error::RelayError,
    types::RelayParameters,
};

// Generate contract bindings from Solidity interface
sol! {
    #[sol(rpc)]
    interface IEncryptedRelayConfig {
        function encryptionPubkey() external view returns (bytes32);
        function previousEncryptionPubkey() external view returns (bytes32);
        function attestationPubkey() external view returns (bytes32);
        function powDifficulty() external view returns (uint8);
        function attestationValiditySeconds() external view returns (uint64);
        function isValidEncryptionKey(bytes32 pubkey) external view returns (bool);
        function getConfig() external view returns (
            bytes32 encryptionPubkey,
            bytes32 previousEncryptionPubkey,
            bytes32 attestationPubkey,
            uint8 powDifficulty,
            uint64 attestationValiditySeconds
        );

        event EncryptionKeyRotated(bytes32 indexed newKey, bytes32 previousKey);
        event AttestationKeyUpdated(bytes32 indexed newKey);
        event DifficultyUpdated(uint8 newDifficulty);
        event AttestationValidityUpdated(uint64 newValidity);
    }
}

/// Configuration for the relay config reader.
#[derive(Debug, Clone)]
pub struct RelayConfigReaderConfig {
    /// Address of the EncryptedRelayConfig contract.
    pub contract_address: Address,
    /// How often to poll for config updates (seconds).
    pub poll_interval_secs: u64,
}

impl Default for RelayConfigReaderConfig {
    fn default() -> Self {
        Self {
            contract_address: Address::ZERO,
            poll_interval_secs: 60, // Poll every minute by default
        }
    }
}

/// Cached relay configuration with atomic updates.
#[derive(Debug)]
pub struct RelayConfigCache {
    /// Current cached parameters.
    params: Arc<ArcSwap<RelayParameters>>,
    /// Shutdown signal sender.
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl RelayConfigCache {
    /// Creates a new config cache with initial parameters.
    pub fn new(initial: RelayParameters) -> Self {
        Self {
            params: Arc::new(ArcSwap::from_pointee(initial)),
            shutdown_tx: None,
        }
    }

    /// Creates a new config cache with default parameters.
    ///
    /// Use this for testing or when running without on-chain config.
    pub fn with_defaults(encryption_pubkey: [u8; 32], attestation_pubkey: [u8; 32]) -> Self {
        let params = RelayParameters {
            encryption_pubkey: Bytes::copy_from_slice(&encryption_pubkey),
            previous_encryption_pubkey: None,
            pow_difficulty: crate::pow::DEFAULT_DIFFICULTY,
            attestation_pubkey: Bytes::copy_from_slice(&attestation_pubkey),
            attestation_validity_seconds: 86400, // 24 hours
        };
        Self::new(params)
    }

    /// Returns the current cached parameters.
    pub fn get(&self) -> Arc<RelayParameters> {
        self.params.load_full()
    }

    /// Updates the cached parameters.
    pub fn update(&self, params: RelayParameters) {
        self.params.store(Arc::new(params));
    }

    /// Checks if a given encryption pubkey is valid (current or previous).
    pub fn is_valid_encryption_key(&self, pubkey: &[u8]) -> bool {
        let params = self.params.load();

        if pubkey == params.encryption_pubkey.as_ref() {
            return true;
        }

        if let Some(ref prev) = params.previous_encryption_pubkey {
            if pubkey == prev.as_ref() {
                return true;
            }
        }

        false
    }

    /// Starts background polling for config updates.
    ///
    /// Returns a handle that stops polling when dropped.
    pub fn start_polling<P: Provider + Clone + Send + Sync + 'static>(
        &mut self,
        provider: P,
        config: RelayConfigReaderConfig,
    ) {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        let params = self.params.clone();
        let contract_address = config.contract_address;
        let poll_interval = std::time::Duration::from_secs(config.poll_interval_secs);

        tokio::spawn(async move {
            info!(
                contract = %contract_address,
                interval_secs = config.poll_interval_secs,
                "Starting relay config polling"
            );

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        info!("Relay config polling stopped");
                        break;
                    }
                    _ = tokio::time::sleep(poll_interval) => {
                        match fetch_config(&provider, contract_address).await {
                            Ok(new_params) => {
                                info!("Fetched relay config from chain");
                                params.store(Arc::new(new_params));
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed to fetch relay config");
                            }
                        }
                    }
                }
            }
        });
    }

    /// Stops background polling.
    pub fn stop_polling(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
    }
}

impl Drop for RelayConfigCache {
    fn drop(&mut self) {
        self.stop_polling();
    }
}

/// Fetches relay config from the on-chain contract.
pub async fn fetch_config<P: Provider>(
    provider: &P,
    contract_address: Address,
) -> Result<RelayParameters, RelayError> {
    let contract = IEncryptedRelayConfig::new(contract_address, provider);

    let config = contract
        .getConfig()
        .call()
        .await
        .map_err(|e| RelayError::ConfigError(format!("failed to call getConfig: {e}")))?;

    let encryption_pubkey = Bytes::copy_from_slice(config.encryptionPubkey.as_slice());

    let previous_encryption_pubkey = if config.previousEncryptionPubkey == B256::ZERO {
        None
    } else {
        Some(Bytes::copy_from_slice(config.previousEncryptionPubkey.as_slice()))
    };

    let attestation_pubkey = Bytes::copy_from_slice(config.attestationPubkey.as_slice());

    Ok(RelayParameters {
        encryption_pubkey,
        previous_encryption_pubkey,
        pow_difficulty: config.powDifficulty,
        attestation_pubkey,
        attestation_validity_seconds: config.attestationValiditySeconds,
    })
}

/// Fetches config once and creates a cache.
pub async fn create_config_cache<P: Provider>(
    provider: &P,
    contract_address: Address,
) -> Result<RelayConfigCache, RelayError> {
    let params = fetch_config(provider, contract_address).await?;
    Ok(RelayConfigCache::new(params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_cache_validation() {
        let key1 = [1u8; 32];
        let key2 = [2u8; 32];
        let attestation = [3u8; 32];

        let params = RelayParameters {
            encryption_pubkey: Bytes::copy_from_slice(&key1),
            previous_encryption_pubkey: Some(Bytes::copy_from_slice(&key2)),
            pow_difficulty: 18,
            attestation_pubkey: Bytes::copy_from_slice(&attestation),
            attestation_validity_seconds: 86400,
        };

        let cache = RelayConfigCache::new(params);

        // Current key is valid
        assert!(cache.is_valid_encryption_key(&key1));

        // Previous key is valid
        assert!(cache.is_valid_encryption_key(&key2));

        // Unknown key is invalid
        assert!(!cache.is_valid_encryption_key(&[4u8; 32]));
    }

    #[test]
    fn test_config_cache_update() {
        let key1 = [1u8; 32];
        let attestation = [2u8; 32];

        let cache = RelayConfigCache::with_defaults(key1, attestation);

        assert_eq!(cache.get().pow_difficulty, crate::pow::DEFAULT_DIFFICULTY);

        // Update with new difficulty
        let mut new_params = (*cache.get()).clone();
        new_params.pow_difficulty = 20;
        cache.update(new_params);

        assert_eq!(cache.get().pow_difficulty, 20);
    }
}
