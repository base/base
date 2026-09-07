use reth_storage_api::BaseBodyStorage;

/// Provides the Base block-body reader.
pub trait ChainStorage: Send + Sync {
    /// Returns the Base block-body reader.
    fn reader(&self) -> &BaseBodyStorage;
}

impl ChainStorage for BaseBodyStorage {
    fn reader(&self) -> &BaseBodyStorage {
        self
    }
}
