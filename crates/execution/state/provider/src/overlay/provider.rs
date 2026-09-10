use std::{sync::Arc, time::Instant};

use alloy_primitives::{B256, BlockHash};
use base_common_observability_metrics::Metrics;
use base_common_types_chain::{self as dashmap, DashMap};
use base_execution_state_database::{
    DBProvider, DatabaseError, DatabaseProviderFactory, DatabaseProviderROFactory, DbTx,
    DbTxProvider,
};
use base_execution_state_trie::{
    DatabaseAccountTrieCursor, DatabaseHashedCursorFactory, DatabaseStorageTrieCursor,
    HashedPostStateSorted, PackedAccountsTrie, PackedKeyAdapter, PackedStoragesTrie,
    hashed_cursor::{HashedCursorFactory, HashedPostStateCursorFactory},
    trie_cursor::{InMemoryTrieCursor, TrieCursor, TrieCursorFactory, TrieStorageCursor},
};
use base_execution_state_types::{
    BlockNumReader, ChangeSetReader, ProviderResult, PruneCheckpointReader, StageCheckpointReader,
    StorageChangeSetReader, StorageSettingsCache,
};
use metrics::{Counter, Histogram};
use tracing::instrument;

use crate::overlay::{Overlay, OverlayBuilder, database_state_frontiers};

/// Metrics for overlay state provider factory operations.
#[derive(Clone, Metrics)]
#[metrics(scope = "storage.providers.overlay")]
pub(crate) struct OverlayStateProviderFactoryMetrics {
    /// Duration of creating the database provider transaction.
    create_provider_duration: Histogram,
    /// Overall duration of the [`OverlayStateProviderFactory::database_provider_ro`] call.
    database_provider_ro_duration: Histogram,
    /// Number of cache misses when fetching [`Overlay`]s from the overlay cache.
    overlay_cache_misses: Counter,
}

/// Factory for creating overlay state providers with optional reverts and overlays.
///
/// This factory allows building an `OverlayStateProvider` whose DB state has been reverted to a
/// particular block, and/or with additional overlay information added on top.
#[derive(Debug, Clone)]
pub struct OverlayStateProviderFactory<F> {
    /// The underlying database provider factory
    factory: F,
    /// Overlay builder containing the configuration and overlay calculation logic.
    overlay_builder: OverlayBuilder,
    /// A cache which maps `(state_trie_tip, finish_tip) -> Overlay`.
    ///
    /// Under partial persistence the overlay depends on both durable frontiers, so both hashes are
    /// part of the cache key.
    overlay_cache: Arc<DashMap<(BlockHash, BlockHash), Overlay>>,
    /// Metrics for provider factory operations.
    metrics: OverlayStateProviderFactoryMetrics,
}

impl<F> OverlayStateProviderFactory<F> {
    /// Create a new overlay state provider factory
    pub fn new(factory: F, overlay_builder: OverlayBuilder) -> Self {
        Self {
            factory,
            overlay_builder,
            overlay_cache: Default::default(),
            metrics: Default::default(),
        }
    }

    /// Skips managed overlay construction when this factory is used by a task that reused a sparse
    /// trie covering both durable frontiers through the parent.
    pub fn with_skip_overlay_for_reused_sparse_trie(mut self, anchor_hash: B256) -> Self {
        self.overlay_builder =
            self.overlay_builder.with_skip_overlay_for_reused_sparse_trie(anchor_hash);
        self.overlay_cache = Default::default();
        self
    }

    /// Fetches an [`Overlay`] from the cache based on the current durable frontiers. If there is no
    /// cached value then this calculates the [`Overlay`] and populates the cache.
    #[instrument(level = "debug", target = "providers::state::overlay", skip_all)]
    fn get_overlay<Provider>(&self, provider: &Provider) -> ProviderResult<Overlay>
    where
        Provider: StageCheckpointReader
            + PruneCheckpointReader
            + ChangeSetReader
            + StorageChangeSetReader
            + DBProvider
            + BlockNumReader
            + StorageSettingsCache,
    {
        let (state_trie_tip_block, finish_tip_block) = database_state_frontiers(provider)?;

        let overlay =
            match self.overlay_cache.entry((state_trie_tip_block.hash, finish_tip_block.hash)) {
                dashmap::Entry::Occupied(entry) => entry.get().clone(),
                dashmap::Entry::Vacant(entry) => {
                    self.metrics.overlay_cache_misses.increment(1);
                    let overlay = self.overlay_builder.build_overlay_at_frontiers(
                        provider,
                        state_trie_tip_block,
                        finish_tip_block,
                    )?;
                    entry.insert(overlay.clone());
                    overlay
                }
            };

        Ok(overlay)
    }
}

impl<F> DatabaseProviderROFactory for OverlayStateProviderFactory<F>
where
    F: DatabaseProviderFactory,
    F::Provider: StageCheckpointReader
        + PruneCheckpointReader
        + BlockNumReader
        + ChangeSetReader
        + StorageChangeSetReader
        + StorageSettingsCache,
{
    type Provider = OverlayStateProvider<F::Provider>;

    /// Create a read-only [`OverlayStateProvider`].
    #[instrument(level = "debug", target = "providers::state::overlay", skip_all)]
    fn database_provider_ro(&self) -> ProviderResult<OverlayStateProvider<F::Provider>> {
        let overall_start = Instant::now();

        // Get a read-only provider
        let provider = {
            let start = Instant::now();
            let res = self.factory.database_provider_ro()?;
            self.metrics.create_provider_duration.record(start.elapsed());
            res
        };

        let overlay = self.get_overlay(&provider)?;

        self.metrics.database_provider_ro_duration.record(overall_start.elapsed());
        Ok(OverlayStateProvider::new(provider, overlay))
    }
}

/// State provider with in-memory overlay from trie updates and hashed post state.
///
/// This provider uses in-memory trie updates and hashed post state as an overlay
/// on top of a database provider, implementing [`TrieCursorFactory`] and [`HashedCursorFactory`]
/// using the in-memory overlay factories.
#[derive(Debug)]
pub struct OverlayStateProvider<Provider> {
    provider: Provider,
    overlay: Overlay,
}

impl<Provider> OverlayStateProvider<Provider> {
    /// Creates a new overlay state provider.
    pub const fn new(provider: Provider, overlay: Overlay) -> Self {
        Self { provider, overlay }
    }
}

impl<Provider> TrieCursorFactory for OverlayStateProvider<Provider>
where
    Provider: DbTxProvider,
{
    type AccountTrieCursor<'a>
        = InMemoryTrieCursor<'a, Box<dyn TrieCursor + Send + 'a>>
    where
        Self: 'a;

    type StorageTrieCursor<'a>
        = InMemoryTrieCursor<'a, Box<dyn TrieStorageCursor + Send + 'a>>
    where
        Self: 'a;

    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor<'_>, DatabaseError> {
        let cursor: Box<dyn TrieCursor + Send> = {
            Box::new(DatabaseAccountTrieCursor::<_, PackedKeyAdapter>::new(
                self.provider.tx().cursor_read::<PackedAccountsTrie>()?,
            ))
        };
        Ok(InMemoryTrieCursor::new_account(cursor, &self.overlay.trie_updates))
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        let cursor: Box<dyn TrieStorageCursor + Send> = {
            Box::new(DatabaseStorageTrieCursor::<_, PackedKeyAdapter>::new(
                self.provider.tx().cursor_dup_read::<PackedStoragesTrie>()?,
                hashed_address,
            ))
        };
        Ok(InMemoryTrieCursor::new_storage(cursor, &self.overlay.trie_updates, hashed_address))
    }
}

impl<Provider> HashedCursorFactory for OverlayStateProvider<Provider>
where
    Provider: DbTxProvider,
{
    type AccountCursor<'a>
        = <HashedPostStateCursorFactory<
        DatabaseHashedCursorFactory<&'a Provider::Tx>,
        &'a Arc<HashedPostStateSorted>,
    > as HashedCursorFactory>::AccountCursor<'a>
    where
        Self: 'a;

    type StorageCursor<'a>
        = <HashedPostStateCursorFactory<
        DatabaseHashedCursorFactory<&'a Provider::Tx>,
        &'a Arc<HashedPostStateSorted>,
    > as HashedCursorFactory>::StorageCursor<'a>
    where
        Self: 'a;

    fn hashed_account_cursor(&self) -> Result<Self::AccountCursor<'_>, DatabaseError> {
        HashedPostStateCursorFactory::new(
            DatabaseHashedCursorFactory::new(self.provider.tx()),
            &self.overlay.hashed_post_state,
        )
        .hashed_account_cursor()
    }

    fn hashed_storage_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageCursor<'_>, DatabaseError> {
        HashedPostStateCursorFactory::new(
            DatabaseHashedCursorFactory::new(self.provider.tx()),
            &self.overlay.hashed_post_state,
        )
        .hashed_storage_cursor(hashed_address)
    }
}
