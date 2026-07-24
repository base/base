//! Storage metadata helpers for selecting the correct trie key encoding.

use eyre::{Result, WrapErr};
use reth_db_api::{models::StorageSettings, tables, transaction::DbTx};
use reth_storage_api::metadata::keys::STORAGE_SETTINGS;

/// Trie key-encoding version configured in the target datadir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageTrieVersion {
    /// Legacy v1 key layout (`StoredNibbles` / `StoredNibblesSubKey`).
    V1,
    /// Packed v2 key layout (`PackedStoredNibbles` / `PackedStoredNibblesSubKey`).
    V2,
}

impl StorageTrieVersion {
    /// Detects trie key encoding from persisted `StorageSettings` metadata.
    ///
    /// This mirrors reth's `MetadataProvider::storage_settings()` behavior:
    /// - key: `storage_settings`
    /// - payload: JSON-encoded `StorageSettings`
    /// - decode failures: treated as absent settings (fallback to v1)
    pub fn detect(tx: &impl DbTx) -> Result<Self> {
        let storage_settings = tx
            .get::<tables::Metadata>(STORAGE_SETTINGS.to_string())
            .wrap_err("read storage settings metadata")?
            .and_then(|bytes| serde_json::from_slice::<StorageSettings>(&bytes).ok());

        Ok(if storage_settings.is_some_and(|settings| settings.is_v2()) {
            Self::V2
        } else {
            Self::V1
        })
    }
}
