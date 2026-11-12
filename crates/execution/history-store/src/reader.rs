//! Archive reader library used inside the node (TDD §5).
//!
//! Every lookup takes an [`ArchiveSnapshot`] resolved once per request and refuses blocks
//! outside `E..=H`. Data read from segment files is checked against its header before it is
//! returned: header hash, transaction root and receipt root. A mismatch is reported as a corrupt
//! segment, never as data. Historical state follows TDD §2.2: find the first change of the key
//! after the block in the archive index, return the changeset's before-value, and otherwise fall
//! back to the state the caller supplies (the node's view at `H`).

use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::{
    BlockBody, Header, TxReceipt, proofs::calculate_transaction_root,
    transaction::SignerRecoverable,
};
use alloy_primitives::{Address, B256, Bytes, Log, U256};
use alloy_rpc_types_eth::Filter;
use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::calculate_receipt_root_no_memo;
use object_store::ObjectStore;
use reth_primitives_traits::{Account, RecoveredBlock};
use reth_provider::{
    ChangeSetReader, HeaderProvider, ReceiptProvider, StorageChangeSetReader, TransactionsProvider,
};
use reth_storage_api::StateProvider;

use crate::{
    ArchiveLayout, ArchiveSnapshot, BodyIndices, CacheConfig, CatalogStore, HashedKey,
    HistoryStoreError, IndexOptions, IndexReader, LoadedSegment, Route, SegmentCache,
    SegmentObjects, SegmentVerification,
};

/// Reader configuration.
#[derive(Debug, Clone)]
pub struct ReaderConfig {
    /// Expected chain id; the reader refuses an archive for another chain.
    pub chain_id: u64,
    /// Expected genesis hash.
    pub genesis_hash: B256,
    /// Chain spec used to check receipt roots.
    pub chain_spec: Arc<BaseChainSpec>,
    /// Segment cache sizing.
    pub cache: CacheConfig,
    /// Index reader options.
    pub index: IndexOptions,
    /// How long a resolved [`ArchiveSnapshot`] is reused before the catalog is read again.
    pub snapshot_ttl: Duration,
}

/// Default [`ReaderConfig::snapshot_ttl`]. `H` only grows and retired segments outlive a grace
/// period far longer than this, so a snapshot this old routes every block correctly.
pub const DEFAULT_SNAPSHOT_TTL: Duration = Duration::from_secs(2);

/// A block read from the archive, checked against its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedBlock {
    /// Header.
    pub header: Header,
    /// Block hash.
    pub hash: B256,
    /// Global transaction numbering.
    pub body_indices: BodyIndices,
    /// Transactions.
    pub transactions: Vec<BaseTxEnvelope>,
    /// Transaction senders.
    pub senders: Vec<Address>,
}

impl ArchivedBlock {
    /// Rebuilds the reth block. The archive does not store ommers or withdrawals: ommers are
    /// always empty on Base, and the body carries an empty withdrawal list whenever the header
    /// commits to withdrawals (post-Canyon), matching what the node serves.
    pub fn into_recovered(self) -> RecoveredBlock<BaseBlock> {
        let withdrawals = self.header.withdrawals_root.map(|_| Default::default());
        let body = BlockBody { transactions: self.transactions, ommers: Vec::new(), withdrawals };
        let block = BaseBlock::new(self.header, body);
        RecoveredBlock::new_unhashed(block, self.senders)
    }
}

/// A log found by [`ArchiveReader::logs`], with its position in the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedLog {
    /// Block number.
    pub block_number: u64,
    /// Block hash.
    pub block_hash: B256,
    /// Transaction hash.
    pub tx_hash: B256,
    /// Transaction index in the block.
    pub tx_index: u64,
    /// Log index in the block.
    pub log_index: u64,
    /// The log.
    pub log: Log,
}

/// The matching logs of one archived block, with the block they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedBlockLogs {
    /// The block, already checked; callers reuse it instead of reading it again.
    pub block: ArchivedBlock,
    /// Logs of the block that match the filter, in log-index order.
    pub logs: Vec<ArchivedLog>,
}

/// Result of a historical state lookup in the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryLookup<T> {
    /// The key changed after the requested block; this is its value at that block.
    Changed(T),
    /// No change up to `H`: the value equals the node's state at `H`.
    Unchanged,
}

impl HistoryLookup<Option<Account>> {
    /// Resolves the account, reading `state_at_h` (the node's state at `H`) when unchanged.
    pub fn or_state(
        self,
        state_at_h: &dyn StateProvider,
        address: Address,
    ) -> Result<Option<Account>, HistoryStoreError> {
        match self {
            Self::Changed(account) => Ok(account),
            Self::Unchanged => Ok(state_at_h.basic_account(&address)?),
        }
    }

    /// Resolves the account's code; bytecode itself always comes from the node (TDD §2.2).
    pub fn code_or_state(
        self,
        state_at_h: &dyn StateProvider,
        address: Address,
    ) -> Result<Bytes, HistoryStoreError> {
        let Some(code_hash) =
            self.or_state(state_at_h, address)?.and_then(|account| account.bytecode_hash)
        else {
            return Ok(Bytes::new());
        };
        Ok(state_at_h
            .bytecode_by_hash(&code_hash)?
            .map(|code| code.original_bytes())
            .unwrap_or_default())
    }
}

impl HistoryLookup<U256> {
    /// Resolves the slot, reading `state_at_h` when unchanged.
    pub fn or_state(
        self,
        state_at_h: &dyn StateProvider,
        address: Address,
        slot: B256,
    ) -> Result<U256, HistoryStoreError> {
        match self {
            Self::Changed(value) => Ok(value),
            Self::Unchanged => Ok(state_at_h.storage(address, slot)?.unwrap_or_default()),
        }
    }
}

/// Reads blocks, transactions, receipts, logs and historical state from the archive.
#[derive(Debug)]
pub struct ArchiveReader {
    /// Catalog access.
    pub catalog: CatalogStore,
    /// Index reader.
    pub index: IndexReader,
    /// Segment cache.
    pub cache: SegmentCache,
    /// Configuration.
    pub config: ReaderConfig,
    /// Highest index watermark seen plus one (0: none yet). Watermarks only grow, so a cached
    /// value at or above `H` proves coverage without another index read.
    pub covered: AtomicU64,
    /// Last resolved snapshot and when it was resolved, reused for
    /// [`ReaderConfig::snapshot_ttl`].
    pub snapshot: Mutex<Option<(Instant, ArchiveSnapshot)>>,
}

impl ArchiveReader {
    /// Opens the reader. Fails if the catalog is unreachable or belongs to another chain or
    /// format; callers serve local data and retry in the background in that case.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        layout: ArchiveLayout,
        config: ReaderConfig,
    ) -> Result<Self, HistoryStoreError> {
        let catalog = CatalogStore::new(Arc::clone(&store), layout.clone());
        let current = catalog.load().await?.ok_or_else(|| HistoryStoreError::Unavailable {
            reason: "catalog.json missing".to_string(),
        })?;
        current.catalog.verify(config.chain_id, config.genesis_hash)?;
        let index = IndexReader::open(Arc::clone(&store), &layout, config.index).await?;
        let cache = SegmentCache::new(
            config.cache.clone(),
            SegmentObjects::new(store, layout),
            &current.catalog,
        )?;
        Ok(Self {
            catalog,
            index,
            cache,
            config,
            covered: AtomicU64::new(0),
            snapshot: Mutex::new(None),
        })
    }

    /// Resolves the archive bounds for one request, reading the catalog at most once per
    /// [`ReaderConfig::snapshot_ttl`].
    pub async fn snapshot(&self) -> Result<ArchiveSnapshot, HistoryStoreError> {
        if let Some((at, snapshot)) = *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner)
            && at.elapsed() < self.config.snapshot_ttl
        {
            return Ok(snapshot);
        }
        let current = self.catalog.load_required().await?;
        current.catalog.verify(self.config.chain_id, self.config.genesis_hash)?;
        let snapshot = ArchiveSnapshot::from_catalog(&current.catalog)?;
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), snapshot));
        Ok(snapshot)
    }

    /// Loads the segment holding `number`, failing unless the snapshot routes it to the archive.
    /// The index view must hold the segment's entries first: a catalog `H` can run ahead of the
    /// view, and a segment the view does not cover yet is `Unavailable`, not corrupt.
    pub async fn segment(
        &self,
        snapshot: &ArchiveSnapshot,
        number: u64,
    ) -> Result<Arc<LoadedSegment>, HistoryStoreError> {
        match snapshot.route(number, false) {
            Route::Archive => {
                let n = snapshot.blocks_per_segment;
                let k = number / n;
                self.ensure_index_covers((k + 1) * n - 1).await?;
                self.cache.get(k, &self.index).await
            }
            Route::HistoryPruned => {
                Err(HistoryStoreError::HistoryPruned { block: number, earliest: snapshot.e })
            }
            Route::Local | Route::Unavailable => Err(HistoryStoreError::Unavailable {
                reason: format!("block {number} is above archive H {:?}", snapshot.h),
            }),
        }
    }

    /// Fails with `Unavailable` until this reader's index view covers `h`. Absent index entries
    /// (unknown hash, no later change) are only authoritative once it does.
    pub async fn ensure_index_covers(&self, h: u64) -> Result<(), HistoryStoreError> {
        if self.covered.load(Ordering::Acquire) > h {
            return Ok(());
        }
        self.index.ensure_covers(h).await?;
        self.covered.fetch_max(h + 1, Ordering::AcqRel);
        Ok(())
    }

    /// Returns whether this reader's index view is known to cover `h`, reading the watermark
    /// unless a covering view was already seen.
    pub async fn index_covers(&self, h: u64) -> Result<bool, HistoryStoreError> {
        if self.covered.load(Ordering::Acquire) > h {
            return Ok(true);
        }
        let Some(watermark) = self.index.watermark().await? else { return Ok(false) };
        self.covered.fetch_max(watermark + 1, Ordering::AcqRel);
        Ok(watermark >= h)
    }

    /// Runs an index point lookup whose absence is only authoritative once the view covers `h`.
    /// Coverage is read before the lookup, so a view that advances while the lookup runs cannot
    /// make a stale absence look authoritative. When the view did not cover `h` and the entry
    /// is absent, coverage is checked again and, once it holds, the lookup runs once more on
    /// the covering view.
    pub async fn covered_lookup<T, F, Fut>(
        &self,
        h: u64,
        lookup: F,
    ) -> Result<Option<T>, HistoryStoreError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Option<T>, HistoryStoreError>>,
    {
        let covered = self.index_covers(h).await?;
        if let Some(found) = lookup().await? {
            return Ok(Some(found));
        }
        if covered {
            return Ok(None);
        }
        self.ensure_index_covers(h).await?;
        lookup().await
    }

    /// Runs a static-file read, converting decoder panics on corrupt files into errors.
    pub fn guarded<T>(
        segment: &LoadedSegment,
        read: impl FnOnce() -> Result<T, reth_provider::ProviderError>,
    ) -> Result<T, HistoryStoreError> {
        std::panic::catch_unwind(AssertUnwindSafe(read))
            .map_err(|_| HistoryStoreError::CorruptSegment {
                segment: segment.segment,
                file: "static files".to_string(),
                reason: "decoder panicked".to_string(),
            })?
            .map_err(Into::into)
    }

    /// Builds a corrupt-segment error.
    pub fn corrupt(segment: &LoadedSegment, number: u64, reason: &str) -> HistoryStoreError {
        HistoryStoreError::CorruptSegment {
            segment: segment.segment,
            file: format!("block {number}"),
            reason: reason.to_string(),
        }
    }

    /// Reads and checks the header of block `number`.
    pub async fn header(
        &self,
        snapshot: &ArchiveSnapshot,
        number: u64,
    ) -> Result<(Header, B256), HistoryStoreError> {
        let segment = self.segment(snapshot, number).await?;
        Self::header_in(&segment, number)
    }

    /// Reads a header from a loaded segment and checks its stored hash.
    pub fn header_in(
        segment: &LoadedSegment,
        number: u64,
    ) -> Result<(Header, B256), HistoryStoreError> {
        let sealed = Self::guarded(segment, || segment.provider.sealed_header(number))?
            .ok_or_else(|| Self::corrupt(segment, number, "missing header"))?;
        let hash = sealed.hash();
        let header = sealed.into_header();
        if header.hash_slow() != hash {
            return Err(Self::corrupt(segment, number, "header does not match its stored hash"));
        }
        Ok((header, hash))
    }

    /// Body indices of block `number` from `block_bodies.idx`.
    pub fn body_in(segment: &LoadedSegment, number: u64) -> Result<BodyIndices, HistoryStoreError> {
        segment
            .bodies
            .entries
            .get((number - segment.first_block) as usize)
            .copied()
            .ok_or_else(|| Self::corrupt(segment, number, "missing body index"))
    }

    /// Reads block `number` with transactions and senders, checking the transaction root.
    pub async fn block(
        &self,
        snapshot: &ArchiveSnapshot,
        number: u64,
    ) -> Result<ArchivedBlock, HistoryStoreError> {
        let segment = self.segment(snapshot, number).await?;
        Self::blocking(move || Self::block_in(&segment, number)).await
    }

    /// Runs segment reads and their CPU-heavy checks on the blocking pool so archive requests
    /// do not stall the RPC server's async workers.
    pub async fn blocking<T: Send + 'static>(
        work: impl FnOnce() -> Result<T, HistoryStoreError> + Send + 'static,
    ) -> Result<T, HistoryStoreError> {
        tokio::task::spawn_blocking(work).await.map_err(std::io::Error::other)?
    }

    /// Reads block `number` from a loaded segment. The transaction root and senders are checked
    /// once per block while the segment stays loaded ([`SegmentVerification`]).
    pub fn block_in(
        segment: &LoadedSegment,
        number: u64,
    ) -> Result<ArchivedBlock, HistoryStoreError> {
        let (header, hash) = Self::header_in(segment, number)?;
        let body = Self::body_in(segment, number)?;
        let range = body.first_tx_num..body.next_tx_num();
        let transactions =
            Self::guarded(segment, || segment.provider.transactions_by_tx_range(range.clone()))?;
        let senders =
            Self::guarded(segment, || segment.provider.senders_by_tx_range(range.clone()))?;
        if transactions.len() as u64 != body.tx_count || senders.len() as u64 != body.tx_count {
            return Err(Self::corrupt(segment, number, "row count differs from body index"));
        }
        let offset = (number - segment.first_block) as u32;
        let verified = &segment.verified;
        if !SegmentVerification::contains(&verified.bodies, offset) {
            if calculate_transaction_root(&transactions) != header.transactions_root {
                return Err(Self::corrupt(segment, number, "transaction root mismatch"));
            }
            // Senders are not covered by any header root; recover them as the writer did.
            for (tx, sender) in transactions.iter().zip(&senders) {
                if tx.recover_signer().ok() != Some(*sender) {
                    return Err(Self::corrupt(segment, number, "sender does not match signature"));
                }
            }
            verified.record(&verified.bodies, offset);
        }
        Ok(ArchivedBlock { header, hash, body_indices: body, transactions, senders })
    }

    /// Reads the receipts of block `number`, checking the receipt root.
    pub async fn receipts(
        &self,
        snapshot: &ArchiveSnapshot,
        number: u64,
    ) -> Result<Vec<BaseReceipt>, HistoryStoreError> {
        let segment = self.segment(snapshot, number).await?;
        let chain_spec = Arc::clone(&self.config.chain_spec);
        Self::blocking(move || Self::receipts_in(&segment, &chain_spec, number)).await
    }

    /// Reads the receipts of block `number` from a loaded segment. The receipt root is checked
    /// once per block while the segment stays loaded ([`SegmentVerification`]).
    pub fn receipts_in(
        segment: &LoadedSegment,
        chain_spec: &BaseChainSpec,
        number: u64,
    ) -> Result<Vec<BaseReceipt>, HistoryStoreError> {
        let (header, _) = Self::header_in(segment, number)?;
        let body = Self::body_in(segment, number)?;
        let range = body.first_tx_num..body.next_tx_num();
        let receipts = Self::guarded(segment, || segment.provider.receipts_by_tx_range(range))?;
        if receipts.len() as u64 != body.tx_count {
            return Err(Self::corrupt(segment, number, "receipt count differs from body index"));
        }
        let offset = (number - segment.first_block) as u32;
        let verified = &segment.verified;
        if !SegmentVerification::contains(&verified.receipts, offset) {
            let root = calculate_receipt_root_no_memo(&receipts, chain_spec, header.timestamp);
            if root != header.receipts_root {
                return Err(Self::corrupt(segment, number, "receipt root mismatch"));
            }
            verified.record(&verified.receipts, offset);
        }
        Ok(receipts)
    }

    /// Resolves a block hash through `b‖`.
    pub async fn block_number(
        &self,
        snapshot: &ArchiveSnapshot,
        hash: B256,
    ) -> Result<Option<u64>, HistoryStoreError> {
        let Some(h) = snapshot.h else { return Ok(None) };
        let found = self.covered_lookup(h, || self.index.block_number(hash, h)).await?;
        Ok(found.filter(|n| *n >= snapshot.e))
    }

    /// Resolves a transaction hash through `t‖` to (block, index).
    pub async fn transaction_location(
        &self,
        snapshot: &ArchiveSnapshot,
        hash: B256,
    ) -> Result<Option<(u64, u32)>, HistoryStoreError> {
        let Some(h) = snapshot.h else { return Ok(None) };
        let found = self.covered_lookup(h, || self.index.tx_location(hash, h)).await?;
        Ok(found.filter(|(n, _)| *n >= snapshot.e))
    }

    /// Returns the blocks in `from..=to` whose header bloom may contain a log matching `filter`.
    pub async fn bloom_hits(
        &self,
        snapshot: &ArchiveSnapshot,
        filter: &Filter,
        from: u64,
        to: u64,
    ) -> Result<Vec<u64>, HistoryStoreError> {
        let mut hits = Vec::new();
        for number in from..=to {
            let (header, _) = self.header(snapshot, number).await?;
            if filter.matches_bloom(header.logs_bloom) {
                hits.push(number);
            }
        }
        Ok(hits)
    }

    /// Scans header blooms over `from..=to` and reads receipts only for hits (TDD §5 getLogs).
    /// Fails with [`HistoryStoreError::TooManyLogs`] once more than `max_logs` logs match,
    /// suggesting the range up to the last block that fit, as reth's `eth_getLogs` does.
    pub async fn logs(
        &self,
        snapshot: &ArchiveSnapshot,
        filter: &Filter,
        from: u64,
        to: u64,
        max_logs: usize,
    ) -> Result<Vec<ArchivedBlockLogs>, HistoryStoreError> {
        let mut out = Vec::new();
        let mut total = 0;
        for number in self.bloom_hits(snapshot, filter, from, to).await? {
            let block = self.block(snapshot, number).await?;
            let receipts = self.receipts(snapshot, number).await?;
            let mut logs = Vec::new();
            let mut log_index = 0;
            for (tx_index, (tx, receipt)) in block.transactions.iter().zip(&receipts).enumerate() {
                for log in receipt.logs() {
                    if filter.matches(log) {
                        logs.push(ArchivedLog {
                            block_number: number,
                            block_hash: block.hash,
                            tx_hash: tx.tx_hash(),
                            tx_index: tx_index as u64,
                            log_index,
                            log: log.clone(),
                        });
                    }
                    log_index += 1;
                }
            }
            if logs.is_empty() {
                continue;
            }
            total += logs.len();
            if total > max_logs {
                let to = if number == from { from } else { number - 1 };
                return Err(HistoryStoreError::TooManyLogs { max: max_logs, from, to });
            }
            out.push(ArchivedBlockLogs { block, logs });
        }
        Ok(out)
    }

    /// Account at the end of block `number` as far as the archive knows (TDD §2.2): the
    /// before-value of the first change after `number` up to `H`, or
    /// [`HistoryLookup::Unchanged`] when the caller must read the node's state at `H`.
    pub async fn account_history(
        &self,
        snapshot: &ArchiveSnapshot,
        address: Address,
        number: u64,
    ) -> Result<HistoryLookup<Option<Account>>, HistoryStoreError> {
        let h = self.state_route(snapshot, number)?;
        // A view missing a segment below H would skip that segment's changes, so coverage is
        // required before the scan, not only when it finds nothing.
        self.ensure_index_covers(h).await?;
        let key = HashedKey::account(address);
        let n = snapshot.blocks_per_segment;
        let Some(change) = self.index.first_change_after(&key, number, n, h).await? else {
            return Ok(HistoryLookup::Unchanged);
        };
        let segment = self.segment(snapshot, change).await?;
        let changes = Self::guarded(&segment, || segment.provider.account_block_changeset(change))?;
        changes
            .into_iter()
            .find(|entry| entry.address == address)
            .map(|entry| HistoryLookup::Changed(entry.info))
            .ok_or_else(|| Self::corrupt(&segment, change, "indexed account change missing"))
    }

    /// Storage slot at the end of block `number` as far as the archive knows (TDD §2.2).
    pub async fn storage_history(
        &self,
        snapshot: &ArchiveSnapshot,
        address: Address,
        slot: B256,
        number: u64,
    ) -> Result<HistoryLookup<U256>, HistoryStoreError> {
        let h = self.state_route(snapshot, number)?;
        self.ensure_index_covers(h).await?;
        let key = HashedKey::storage(address, slot);
        let n = snapshot.blocks_per_segment;
        let Some(change) = self.index.first_change_after(&key, number, n, h).await? else {
            return Ok(HistoryLookup::Unchanged);
        };
        let segment = self.segment(snapshot, change).await?;
        let changes = Self::guarded(&segment, || segment.provider.storage_block_changeset(change))?;
        changes
            .into_iter()
            .find(|entry| entry.address == address && entry.key == slot)
            .map(|entry| HistoryLookup::Changed(entry.value))
            .ok_or_else(|| Self::corrupt(&segment, change, "indexed storage change missing"))
    }

    /// Checks a state read at `number` is answerable from the archive and returns `H`.
    pub fn state_route(
        &self,
        snapshot: &ArchiveSnapshot,
        number: u64,
    ) -> Result<u64, HistoryStoreError> {
        match snapshot.route(number, false) {
            Route::Archive => snapshot.require_h(),
            Route::HistoryPruned => {
                Err(HistoryStoreError::HistoryPruned { block: number, earliest: snapshot.e })
            }
            Route::Local | Route::Unavailable => Err(HistoryStoreError::Unavailable {
                reason: format!("state at {number} is above archive H {:?}", snapshot.h),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        num::NonZeroUsize,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use alloy_primitives::keccak256;
    use object_store::{ObjectStoreExt, memory::InMemory};
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    use super::*;
    use crate::test_utils::{
        ChainFixture, FIXTURE_CHAIN_ID, FIXTURE_SLOTS, GateOp, GatedStore, fixture_writer,
        test_index_options,
    };

    /// Publishes `segments` segments of 4 blocks and returns the store and fixture.
    async fn published(segments: u64) -> (Arc<dyn ObjectStore>, ChainFixture) {
        let fixture = ChainFixture::new(segments * 4, 2);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = fixture_writer(Arc::clone(&store), &fixture, 4).await;
        for _ in 0..segments {
            writer.step().await.unwrap();
        }
        writer.index.close().await.unwrap();
        (store, fixture)
    }

    /// Reader configuration for the fixture chain.
    fn config(fixture: &ChainFixture, cache: CacheConfig) -> ReaderConfig {
        ReaderConfig {
            chain_id: FIXTURE_CHAIN_ID,
            genesis_hash: fixture.blocks[0].hash,
            chain_spec: Arc::clone(&fixture.chain_spec),
            cache,
            index: test_index_options(),
            snapshot_ttl: Duration::ZERO,
        }
    }

    /// The node's state at H, built from the fixture's final state.
    fn state_at(fixture: &ChainFixture, block: usize) -> MockEthProvider {
        let provider = MockEthProvider::default();
        let state = &fixture.states[block];
        for (address, account) in &state.accounts {
            let mut extended = ExtendedAccount::new(account.nonce, account.balance);
            if *address == fixture.contract {
                extended = extended.with_bytecode(Bytes::from_static(&[0x60, 0x00]));
                let slots = state
                    .storage
                    .iter()
                    .filter(|((owner, _), _)| owner == address)
                    .map(|((_, slot), value)| (*slot, *value));
                extended = extended.extend_storage(slots);
            }
            provider.add_account(*address, extended);
        }
        provider
    }

    /// Every block, receipt, hash lookup and log read back equals the fixture.
    #[tokio::test]
    async fn reads_blocks_receipts_and_hashes() {
        let (store, fixture) = published(3).await;
        let reader = ArchiveReader::open(
            store,
            ArchiveLayout::new("chain"),
            config(&fixture, CacheConfig::default()),
        )
        .await
        .unwrap();
        let snapshot = reader.snapshot().await.unwrap();
        assert_eq!(
            snapshot,
            ArchiveSnapshot { h: Some(11), e: 0, blocks_per_segment: 4 },
            "snapshot"
        );
        for expected in &fixture.blocks {
            let n = expected.number();
            let block = reader.block(&snapshot, n).await.unwrap();
            assert_eq!(block.header, expected.header, "header {n}");
            assert_eq!(block.hash, expected.hash, "hash {n}");
            assert_eq!(block.transactions, expected.transactions, "transactions {n}");
            assert_eq!(block.senders, expected.senders, "senders {n}");
            assert_eq!(
                reader.receipts(&snapshot, n).await.unwrap(),
                expected.receipts,
                "receipts {n}"
            );
            assert_eq!(
                reader.block_number(&snapshot, expected.hash).await.unwrap(),
                Some(n),
                "b‖ {n}"
            );
            for (i, tx) in expected.transactions.iter().enumerate() {
                assert_eq!(
                    reader.transaction_location(&snapshot, tx.tx_hash()).await.unwrap(),
                    Some((n, i as u32)),
                    "t‖ {n}/{i}"
                );
            }
        }
        let err = reader.block(&snapshot, 12).await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Unavailable { .. }),
            "above H is unavailable: {err}"
        );
    }

    /// Logs are found through blooms and match a direct scan of the fixture.
    #[tokio::test]
    async fn logs_match_fixture_scan() {
        let (store, fixture) = published(3).await;
        let reader = ArchiveReader::open(
            store,
            ArchiveLayout::new("chain"),
            config(&fixture, CacheConfig::default()),
        )
        .await
        .unwrap();
        let snapshot = reader.snapshot().await.unwrap();
        let topic = B256::with_last_byte(1);
        let filter = Filter::new().address(fixture.contract).event_signature(topic);
        let blocks = reader.logs(&snapshot, &filter, 2, 9, usize::MAX).await.unwrap();
        for group in &blocks {
            assert!(
                group.logs.iter().all(|log| log.block_number == group.block.header.number),
                "each group holds only its own block's logs"
            );
        }
        let logs: Vec<_> = blocks.iter().flat_map(|group| &group.logs).collect();
        let mut expected = Vec::new();
        for block in &fixture.blocks[2..=9] {
            for (i, receipt) in block.receipts.iter().enumerate() {
                for log in receipt.logs() {
                    if log.topics()[0] == topic {
                        expected.push((block.number(), i as u64, log.clone()));
                    }
                }
            }
        }
        assert_eq!(
            logs.iter().map(|l| (l.block_number, l.tx_index, l.log.clone())).collect::<Vec<_>>(),
            expected,
            "bloom-filtered logs equal the fixture scan"
        );
        assert_eq!(expected.len(), 5, "fixture has 5 matching logs in blocks 2..=9");
    }

    /// Checks run so far across the fixture's three loaded segments.
    async fn verification_checks(reader: &ArchiveReader, snapshot: &ArchiveSnapshot) -> u64 {
        let mut total = 0;
        for k in 0..3 {
            let segment = reader.segment(snapshot, k * 4).await.unwrap();
            total += segment.verified.checks.load(Ordering::Relaxed);
        }
        total
    }

    /// Repeated log reads check each hit block's transaction root, senders and receipt root
    /// once while its segment stays loaded, and later block reads reuse those checks.
    #[tokio::test]
    async fn log_reads_verify_each_block_once_per_segment_load() {
        let (store, fixture) = published(3).await;
        let reader = ArchiveReader::open(
            store,
            ArchiveLayout::new("chain"),
            config(&fixture, CacheConfig::default()),
        )
        .await
        .unwrap();
        let snapshot = reader.snapshot().await.unwrap();
        let filter =
            Filter::new().address(fixture.contract).event_signature(B256::with_last_byte(1));
        let first = reader.logs(&snapshot, &filter, 0, 11, usize::MAX).await.unwrap();
        let hits: Vec<u64> = first.iter().map(|group| group.block.header.number).collect();
        assert!(!hits.is_empty(), "the fixture has matching logs");
        assert_eq!(
            verification_checks(&reader, &snapshot).await,
            2 * hits.len() as u64,
            "one body and one receipt check per hit block {hits:?}"
        );
        let second = reader.logs(&snapshot, &filter, 0, 11, usize::MAX).await.unwrap();
        assert_eq!(second, first, "a repeated query returns the same logs");
        for number in &hits {
            reader.block(&snapshot, *number).await.unwrap();
            reader.receipts(&snapshot, *number).await.unwrap();
        }
        assert_eq!(
            verification_checks(&reader, &snapshot).await,
            2 * hits.len() as u64,
            "repeated reads of the same blocks run no new checks"
        );
    }

    /// More matching logs than the cap fail with reth's retry range: from the requested start
    /// to the block before the one that crossed the cap.
    #[tokio::test]
    async fn logs_over_the_cap_fail_with_a_retry_range() {
        let (store, fixture) = published(3).await;
        let reader = ArchiveReader::open(
            store,
            ArchiveLayout::new("chain"),
            config(&fixture, CacheConfig::default()),
        )
        .await
        .unwrap();
        let snapshot = reader.snapshot().await.unwrap();
        let filter = Filter::new().address(fixture.contract);
        let all = reader.logs(&snapshot, &filter, 0, 11, usize::MAX).await.unwrap();
        let counts: Vec<(u64, usize)> =
            all.iter().map(|group| (group.block.header.number, group.logs.len())).collect();
        assert!(counts.len() >= 2, "the fixture has logs in at least two blocks: {counts:?}");
        let max = counts[0].1;
        let err = reader.logs(&snapshot, &filter, 0, 11, max).await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::TooManyLogs { max: m, from: 0, to } if m == max && to == counts[1].0 - 1),
            "the cap is crossed at block {}, got {err}",
            counts[1].0
        );
        let fits = reader.logs(&snapshot, &filter, 0, counts[1].0 - 1, max).await.unwrap();
        assert_eq!(fits.len(), 1, "the suggested retry range fits under the cap");
    }

    /// Historical account, storage and code values match the replayed fixture state at every
    /// block, through changesets or the current-state fallback.
    #[tokio::test]
    async fn historical_state_matches_fixture() {
        let (store, fixture) = published(3).await;
        let reader = ArchiveReader::open(
            store,
            ArchiveLayout::new("chain"),
            config(&fixture, CacheConfig::default()),
        )
        .await
        .unwrap();
        let snapshot = reader.snapshot().await.unwrap();
        let state_at_h = state_at(&fixture, 11);
        let mut addresses = fixture.senders.clone();
        addresses.extend([fixture.recipient, fixture.contract, Address::with_last_byte(0x99)]);
        for block in 0..=11u64 {
            let expected = &fixture.states[block as usize];
            for address in &addresses {
                assert_eq!(
                    reader
                        .account_history(&snapshot, *address, block)
                        .await
                        .unwrap()
                        .or_state(&state_at_h, *address)
                        .unwrap(),
                    expected.accounts.get(address).copied(),
                    "account {address} at {block}"
                );
            }
            for slot in 0..FIXTURE_SLOTS {
                let slot = B256::from(U256::from(slot));
                assert_eq!(
                    reader
                        .storage_history(&snapshot, fixture.contract, slot, block)
                        .await
                        .unwrap()
                        .or_state(&state_at_h, fixture.contract, slot)
                        .unwrap(),
                    expected.storage.get(&(fixture.contract, slot)).copied().unwrap_or_default(),
                    "slot {slot} at {block}"
                );
            }
            assert_eq!(
                keccak256(
                    reader
                        .account_history(&snapshot, fixture.contract, block)
                        .await
                        .unwrap()
                        .code_or_state(&state_at_h, fixture.contract)
                        .unwrap()
                ),
                keccak256([0x60, 0x00]),
                "contract code at {block}"
            );
        }
        let err = reader.account_history(&snapshot, fixture.recipient, 12).await.unwrap_err();
        assert!(matches!(err, HistoryStoreError::Unavailable { .. }), "state above H: {err}");
    }

    /// Opens a reader over `store` with the given cache.
    async fn open_reader(
        store: &Arc<dyn ObjectStore>,
        fixture: &ChainFixture,
        cache: CacheConfig,
    ) -> ArchiveReader {
        ArchiveReader::open(Arc::clone(store), ArchiveLayout::new("chain"), config(fixture, cache))
            .await
            .unwrap()
    }

    /// Size in bytes of segment 0's objects.
    async fn segment_size(store: &Arc<dyn ObjectStore>) -> u64 {
        let objects = SegmentObjects::new(Arc::clone(store), ArchiveLayout::new("chain"));
        objects.list(0).await.unwrap().iter().map(|meta| meta.size).sum()
    }

    /// Disk cache config with the given budget under `dir`.
    fn disk(dir: &tempfile::TempDir, disk_bytes: u64) -> CacheConfig {
        CacheConfig {
            memory_segments: NonZeroUsize::new(1).unwrap(),
            disk_dir: Some(dir.path().to_path_buf()),
            disk_bytes,
        }
    }

    /// Published segment entries under the cache namespace.
    fn disk_entries(reader: &ArchiveReader) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(reader.cache.disk_root().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// The memory tier holds exactly its capacity; the disk tier never exceeds its budget and
    /// evicts least recently used segments to fit.
    #[tokio::test]
    async fn cache_tiers_are_bounded() {
        let (store, fixture) = published(3).await;
        let size = segment_size(&store).await;
        let dir = tempfile::tempdir().unwrap();
        let reader = open_reader(&store, &fixture, disk(&dir, size * 2 + size / 2)).await;
        let snapshot = reader.snapshot().await.unwrap();
        for n in [0, 4, 8] {
            reader.block(&snapshot, n).await.unwrap();
        }
        assert_eq!(reader.cache.memory_len(), 1, "memory tier holds exactly 1 segment");
        assert_eq!(
            disk_entries(&reader),
            vec!["seg-1", "seg-2"],
            "budget of 2.5 segments keeps the 2 newest"
        );
        let usage = reader.cache.disk_usage().unwrap();
        assert!(
            usage <= size * 2 + size / 2,
            "disk usage {usage} within budget {}",
            size * 2 + size / 2
        );
        assert_eq!(
            reader.block(&snapshot, 1).await.unwrap().hash,
            fixture.blocks[1].hash,
            "re-fetch after eviction"
        );
        assert_eq!(
            reader.cache.downloads.load(Ordering::Relaxed),
            4,
            "three fills plus one re-fetch"
        );
    }

    /// A segment larger than the whole budget is served from a temporary directory and never
    /// persisted.
    #[tokio::test]
    async fn oversized_segment_is_served_ephemerally() {
        let (store, fixture) = published(1).await;
        let size = segment_size(&store).await;
        let dir = tempfile::tempdir().unwrap();
        let reader = open_reader(&store, &fixture, disk(&dir, size - 1)).await;
        let snapshot = reader.snapshot().await.unwrap();
        assert_eq!(
            reader.block(&snapshot, 2).await.unwrap().hash,
            fixture.blocks[2].hash,
            "served"
        );
        assert_eq!(reader.cache.disk_usage().unwrap(), 0, "nothing persisted");
        assert!(
            !reader.cache.disk_root().unwrap().join("seg-0").exists(),
            "no entry for segment 0"
        );
    }

    /// Disk entries are namespaced by archive identity: a reader of another chain id never
    /// sees this archive's entries.
    #[tokio::test]
    async fn disk_cache_is_namespaced_by_archive() {
        let (store, fixture) = published(1).await;
        let dir = tempfile::tempdir().unwrap();
        let reader = open_reader(&store, &fixture, disk(&dir, 1 << 30)).await;
        let snapshot = reader.snapshot().await.unwrap();
        reader.block(&snapshot, 0).await.unwrap();
        let namespace =
            format!("{FIXTURE_CHAIN_ID}-{}-f{}-n4", fixture.blocks[0].hash, crate::ARCHIVE_FORMAT);
        assert_eq!(reader.cache.namespace, namespace, "namespace is chain id, genesis, format, N");
        let roots: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(roots, vec![namespace], "the cache dir holds exactly this archive's namespace");
    }

    /// Concurrent misses for one segment share one download, and the published entry is
    /// complete (no `fill-*` directory left behind).
    #[tokio::test]
    async fn concurrent_misses_fill_once() {
        let (store, fixture) = published(1).await;
        // Slow gets make the eight reads overlap inside the fill.
        let throttle = object_store::throttle::ThrottleConfig {
            wait_get_per_call: std::time::Duration::from_millis(5),
            ..Default::default()
        };
        let store: Arc<dyn ObjectStore> =
            Arc::new(object_store::throttle::ThrottledStore::new(store, throttle));
        let dir = tempfile::tempdir().unwrap();
        let reader = open_reader(&store, &fixture, disk(&dir, 1 << 30)).await;
        let snapshot = reader.snapshot().await.unwrap();
        let reads = futures::future::join_all((0..8).map(|i| reader.block(&snapshot, i % 4))).await;
        for (i, read) in reads.into_iter().enumerate() {
            assert_eq!(read.unwrap().hash, fixture.blocks[i % 4].hash, "read {i}");
        }
        assert_eq!(
            reader.cache.downloads.load(Ordering::Relaxed),
            1,
            "eight concurrent misses, one download"
        );
        assert_eq!(disk_entries(&reader), vec!["seg-0"], "one complete entry, no fill directory");
        assert!(reader.cache.fills.lock().unwrap().is_empty(), "no fill lock left behind");
    }

    /// A corrupted disk entry fails its manifest check, is discarded and refetched.
    #[tokio::test]
    async fn corrupt_disk_entry_is_refetched() {
        let (store, fixture) = published(1).await;
        let dir = tempfile::tempdir().unwrap();
        let reader = open_reader(&store, &fixture, disk(&dir, 1 << 30)).await;
        let snapshot = reader.snapshot().await.unwrap();
        reader.block(&snapshot, 0).await.unwrap();
        let entry = reader.cache.disk_root().unwrap().join("seg-0");
        let changesets = std::fs::read_dir(&entry)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                crate::SegmentFile::object_name(&p.file_name().unwrap().to_string_lossy())
                    .as_deref()
                    == Some("account_changesets.dat")
            })
            .unwrap_or_else(|| panic!("account changeset file in {entry:?}"));
        let mut raw = std::fs::read(&changesets).unwrap();
        raw[0] ^= 0xff;
        std::fs::write(&changesets, raw).unwrap();
        // A fresh reader has an empty memory tier, so it reads the disk entry.
        let fresh = open_reader(&store, &fixture, disk(&dir, 1 << 30)).await;
        assert_eq!(
            fresh.block(&snapshot, 1).await.unwrap().hash,
            fixture.blocks[1].hash,
            "served after refetch"
        );
        assert_eq!(
            fresh.cache.downloads.load(Ordering::Relaxed),
            1,
            "corrupt entry was refetched once"
        );
    }

    /// A corrupted changeset object in the bucket is rejected by the manifest before any state
    /// is read from it.
    #[tokio::test]
    async fn corrupt_changeset_object_is_rejected() {
        let (store, fixture) = published(1).await;
        let path = ArchiveLayout::new("chain").segment_object(0, 1, "account_changesets.dat");
        let mut raw = store.get(&path).await.unwrap().bytes().await.unwrap().to_vec();
        raw[0] ^= 0xff;
        store.put(&path, raw.into()).await.unwrap();
        let reader = open_reader(&store, &fixture, CacheConfig::default()).await;
        let snapshot = reader.snapshot().await.unwrap();
        let err = reader.account_history(&snapshot, fixture.recipient, 0).await.unwrap_err();
        assert!(
            matches!(&err, HistoryStoreError::CorruptSegment { segment: 0, file, .. } if file == "account_changesets.dat"),
            "corrupt changeset file is rejected, got {err}"
        );
    }

    /// Senders are recovered on read: a sender file rewritten together with its manifest is
    /// still rejected because the signature disagrees.
    #[tokio::test]
    async fn tampered_sender_is_rejected() {
        let (store, fixture) = published(1).await;
        let layout = ArchiveLayout::new("chain");
        let path = layout.segment_object(0, 1, "transaction_senders.dat");
        let mut raw = store.get(&path).await.unwrap().bytes().await.unwrap().to_vec();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        store.put(&path, raw.into()).await.unwrap();
        let objects = SegmentObjects::new(Arc::clone(&store), layout.clone());
        let index = crate::IndexWriter::open(Arc::clone(&store), &layout, test_index_options())
            .await
            .unwrap();
        let published = index.required_manifest(0).await.unwrap();
        let manifest = objects.download(&published, 4).await.unwrap().manifest(published.epoch);
        index.db.put(crate::IndexKey::manifest(0), manifest.encode()).await.unwrap();
        index.db.flush().await.unwrap();
        index.close().await.unwrap();
        let reader = open_reader(&store, &fixture, CacheConfig::default()).await;
        let snapshot = reader.snapshot().await.unwrap();
        let err = reader.block(&snapshot, 3).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            HistoryStoreError::CorruptSegment {
                segment: 0,
                file: "block 3".to_string(),
                reason: "sender does not match signature".to_string(),
            }
            .to_string(),
            "tampered sender is rejected"
        );
    }

    /// When the catalog's H is ahead of the reader's index view, absent entries are
    /// `Unavailable`, never "unknown hash" or `Unchanged`; present entries still resolve.
    #[tokio::test]
    async fn index_view_behind_h_refuses_absent_entries() {
        let (store, fixture) = published(1).await;
        let catalogs = CatalogStore::new(Arc::clone(&store), ArchiveLayout::new("chain"));
        let current = catalogs.load_required().await.unwrap();
        let ahead = crate::Catalog {
            generation: current.catalog.generation + 1,
            h: Some(7),
            ..current.catalog.clone()
        };
        catalogs.update(&current, ahead).await.unwrap();
        let reader = open_reader(&store, &fixture, CacheConfig::default()).await;
        let snapshot = reader.snapshot().await.unwrap();
        assert_eq!(snapshot.h, Some(7), "catalog H is ahead of the index");
        assert_eq!(
            reader.block_number(&snapshot, fixture.blocks[2].hash).await.unwrap(),
            Some(2),
            "an indexed hash still resolves"
        );
        let unavailable = |result: Result<_, HistoryStoreError>, what: &str| {
            let err = result.err().unwrap_or_else(|| panic!("{what} must fail"));
            assert_eq!(
                err.to_string(),
                "archive unavailable: index view at Some(3) trails archive H 7",
                "{what}"
            );
        };
        unavailable(
            reader.block_number(&snapshot, B256::with_last_byte(9)).await.map(|_| ()),
            "unknown block hash",
        );
        // Review2 P2: a number-based read of a segment the view does not cover yet must not
        // reach the cache, where the absent manifest would read as a corrupt segment.
        unavailable(
            reader.header(&snapshot, 5).await.map(|_| ()),
            "header of an uncovered segment",
        );
        unavailable(reader.block(&snapshot, 5).await.map(|_| ()), "block of an uncovered segment");
        unavailable(
            reader.receipts(&snapshot, 5).await.map(|_| ()),
            "receipts of an uncovered segment",
        );
        unavailable(
            reader.transaction_location(&snapshot, B256::with_last_byte(9)).await.map(|_| ()),
            "unknown tx hash",
        );
        unavailable(
            reader.account_history(&snapshot, fixture.recipient, 1).await.map(|_| ()),
            "account history",
        );
        unavailable(
            reader.storage_history(&snapshot, fixture.contract, B256::ZERO, 1).await.map(|_| ()),
            "storage history",
        );
    }

    /// A corrupt object yields an error, never wrong data.
    #[tokio::test]
    async fn corrupt_object_is_an_error() {
        let (store, fixture) = published(1).await;
        let path = ArchiveLayout::new("chain").segment_object(0, 1, "transactions.dat");
        let mut raw = store.get(&path).await.unwrap().bytes().await.unwrap().to_vec();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        store.put(&path, raw.into()).await.unwrap();
        let reader = ArchiveReader::open(
            store,
            ArchiveLayout::new("chain"),
            config(&fixture, CacheConfig::default()),
        )
        .await
        .unwrap();
        let snapshot = reader.snapshot().await.unwrap();
        let err = reader.block(&snapshot, 3).await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::CorruptSegment { segment: 0, .. }),
            "corrupt tx is an error: {err}"
        );
    }

    /// A reader for another chain refuses to open.
    #[tokio::test]
    async fn refuses_foreign_catalog() {
        let (store, fixture) = published(1).await;
        let mut cfg = config(&fixture, CacheConfig::default());
        cfg.chain_id += 1;
        let err = ArchiveReader::open(store, ArchiveLayout::new("chain"), cfg).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "catalog chain_id mismatch: expected {}, got {FIXTURE_CHAIN_ID}",
                FIXTURE_CHAIN_ID + 1
            ),
            "chain id mismatch is refused"
        );
    }

    /// Fill directories left under the cache namespace.
    fn fill_dirs(reader: &ArchiveReader) -> Vec<String> {
        let Some(root) = reader.cache.disk_root().filter(|root| root.exists()) else {
            return Vec::new();
        };
        std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("fill-"))
            .collect()
    }

    /// Bytes of every file under the cache namespace, measured on the filesystem.
    fn measured_bytes(reader: &ArchiveReader) -> u64 {
        fn walk(path: &std::path::Path) -> u64 {
            std::fs::read_dir(path)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    let meta = entry.metadata().unwrap();
                    if meta.is_dir() { walk(&entry.path()) } else { meta.len() }
                })
                .sum()
        }
        reader.cache.disk_root().filter(|root| root.exists()).map_or(0, |root| walk(&root))
    }

    /// Review3 P2: a process that crashed mid-fill leaves a populated `fill-*` directory that
    /// no reservation covers. Reopening the cache removes it, so admitting another segment
    /// keeps the bytes on the filesystem within the budget.
    #[tokio::test]
    async fn abandoned_fill_is_removed_on_open() {
        let (store, fixture) = published(2).await;
        let size = segment_size(&store).await;
        let budget = size + size / 2;
        let dir = tempfile::tempdir().unwrap();
        let reader = open_reader(&store, &fixture, disk(&dir, budget)).await;
        let snapshot = reader.snapshot().await.unwrap();
        reader.block(&snapshot, 0).await.unwrap();
        let root = reader.cache.disk_root().unwrap();
        drop(reader);
        // A crashed fill of segment 0: a complete copy of its files under a `fill-*` name.
        let abandoned = root.join("fill-crashed");
        std::fs::create_dir(&abandoned).unwrap();
        for entry in std::fs::read_dir(root.join("seg-0")).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), abandoned.join(entry.file_name())).unwrap();
        }

        let reader = open_reader(&store, &fixture, disk(&dir, budget)).await;
        assert!(!abandoned.exists(), "the abandoned fill is removed when the cache opens");
        assert_eq!(fill_dirs(&reader), Vec::<String>::new(), "no fill directory remains");
        let snapshot = reader.snapshot().await.unwrap();
        assert_eq!(
            reader.block(&snapshot, 4).await.unwrap().hash,
            fixture.blocks[4].hash,
            "segment 1 served"
        );
        let measured = measured_bytes(&reader);
        assert!(measured <= budget, "measured filesystem bytes {measured} within budget {budget}");
        assert_eq!(
            disk_entries(&reader),
            vec!["seg-1"],
            "segment 1 admitted in place of segment 0"
        );
    }

    /// Review3 P2: entries persisted under a larger budget are reconciled when the cache opens
    /// with a smaller one, before any lease: least recently used entries are evicted until the
    /// rest fit, and an entry larger than the whole budget is removed so its segment is served
    /// ephemerally instead of being leased in place.
    #[tokio::test]
    async fn persisted_entries_follow_a_smaller_budget() {
        let (store, fixture) = published(2).await;
        let size = segment_size(&store).await;
        let dir = tempfile::tempdir().unwrap();
        let reader = open_reader(&store, &fixture, disk(&dir, 1 << 30)).await;
        let snapshot = reader.snapshot().await.unwrap();
        reader.block(&snapshot, 0).await.unwrap();
        reader.block(&snapshot, 4).await.unwrap();
        assert_eq!(disk_entries(&reader), vec!["seg-0", "seg-1"], "both segments persisted");
        drop(reader);
        // Segment 1 is the most recently used entry.
        let root = dir.path().read_dir().unwrap().next().unwrap().unwrap().path();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(root.join("seg-0").join("USED"))
            .unwrap()
            .set_modified(old)
            .unwrap();

        let budget = size + size / 2;
        let reader = open_reader(&store, &fixture, disk(&dir, budget)).await;
        assert_eq!(
            disk_entries(&reader),
            vec!["seg-1"],
            "the least recently used entry is evicted"
        );
        let measured = measured_bytes(&reader);
        assert!(measured <= budget, "measured filesystem bytes {measured} within budget {budget}");
        drop(reader);

        let reader = open_reader(&store, &fixture, disk(&dir, size - 1)).await;
        assert_eq!(
            disk_entries(&reader),
            Vec::<String>::new(),
            "an entry larger than the budget is removed"
        );
        let snapshot = reader.snapshot().await.unwrap();
        assert_eq!(
            reader.block(&snapshot, 4).await.unwrap().hash,
            fixture.blocks[4].hash,
            "segment 1 served"
        );
        assert_eq!(
            measured_bytes(&reader),
            0,
            "segment 1 is served ephemerally, nothing persisted"
        );
    }

    /// Review2 P2 cache lifetime: a loaded segment whose header jar is open, but whose other
    /// jars reth has not opened yet, keeps its disk entry under eviction pressure. Reading its
    /// transactions and changesets afterwards still works, and the fill that could not evict
    /// it is served without exceeding the budget.
    #[tokio::test]
    async fn leased_disk_entry_survives_eviction_pressure() {
        let (store, fixture) = published(2).await;
        let size = segment_size(&store).await;
        let dir = tempfile::tempdir().unwrap();
        let reader = open_reader(&store, &fixture, disk(&dir, size + size / 2)).await;
        let snapshot = reader.snapshot().await.unwrap();
        let held = reader.segment(&snapshot, 0).await.unwrap();
        assert_eq!(
            ArchiveReader::header_in(&held, 0).unwrap().1,
            fixture.blocks[0].hash,
            "header read first"
        );

        assert_eq!(
            reader.block(&snapshot, 4).await.unwrap().hash,
            fixture.blocks[4].hash,
            "segment 1 served"
        );
        assert_eq!(reader.cache.memory_len(), 1, "segment 0 left the memory tier");
        assert_eq!(disk_entries(&reader), vec!["seg-0"], "the leased entry was not evicted");
        let usage = reader.cache.disk_usage().unwrap();
        assert!(usage <= size + size / 2, "disk usage {usage} within budget {}", size + size / 2);

        let body = ArchiveReader::body_in(&held, 1).unwrap();
        let txs =
            held.provider.transactions_by_tx_range(body.first_tx_num..body.next_tx_num()).unwrap();
        assert_eq!(
            txs, fixture.blocks[1].transactions,
            "transactions read after eviction pressure"
        );
        let mut expected = fixture.blocks[1].account_changes.clone();
        expected.sort_by_key(|change| change.address);
        assert_eq!(
            held.provider.account_block_changeset(1).unwrap(),
            expected,
            "changesets read after eviction pressure"
        );

        drop(held);
        assert_eq!(
            reader.block(&snapshot, 0).await.unwrap().hash,
            fixture.blocks[0].hash,
            "segment 0 reloads"
        );
    }

    /// Review2 P2 budget: a fill holds a reservation for its manifest size from before its
    /// download until it publishes, so a concurrent fill of another segment that would exceed
    /// the budget beside it is served without persisting.
    #[tokio::test]
    async fn concurrent_fills_share_the_budget() {
        let (store, fixture) = published(2).await;
        let size = segment_size(&store).await;
        let gated = GatedStore::new(Arc::clone(&store));
        let mut gate = gated.arm(GateOp::Get, "chain/seg/0/");
        let gated: Arc<dyn ObjectStore> = Arc::new(gated);
        let dir = tempfile::tempdir().unwrap();
        let reader = open_reader(&gated, &fixture, disk(&dir, size + size / 2)).await;
        let snapshot = reader.snapshot().await.unwrap();
        let first = reader.block(&snapshot, 0);
        let second = async {
            gate.reached().await;
            assert_eq!(reader.cache.disk_reserved(), size, "the paused fill holds its reservation");
            let block = reader.block(&snapshot, 4).await.unwrap();
            assert_eq!(
                disk_entries(&reader),
                Vec::<String>::new(),
                "segment 1 is not admitted beside it"
            );
            gate.release();
            block
        };
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap().hash, fixture.blocks[0].hash, "segment 0 served");
        assert_eq!(second.hash, fixture.blocks[4].hash, "segment 1 served ephemerally");
        assert_eq!(disk_entries(&reader), vec!["seg-0"], "only the admitted fill persisted");
        assert_eq!(reader.cache.disk_reserved(), 0, "reservations released");
        assert!(reader.cache.disk_usage().unwrap() <= size + size / 2, "budget held");
    }

    /// Review2 P3 cancellation: dropping a read mid-fill releases its fill lock entry and its
    /// budget reservation and leaves no fill directory; the next read fills normally.
    #[tokio::test]
    async fn cancelled_fill_cleans_up() {
        let (store, fixture) = published(1).await;
        let gated = GatedStore::new(Arc::clone(&store));
        let mut gate = gated.arm(GateOp::Get, "chain/seg/0/");
        let gated: Arc<dyn ObjectStore> = Arc::new(gated);
        let dir = tempfile::tempdir().unwrap();
        let reader = Arc::new(open_reader(&gated, &fixture, disk(&dir, 1 << 30)).await);
        let snapshot = reader.snapshot().await.unwrap();
        let task = {
            let reader = Arc::clone(&reader);
            tokio::spawn(async move { reader.block(&snapshot, 0).await.map(|_| ()) })
        };
        gate.reached().await;
        assert_eq!(reader.cache.fills.lock().unwrap().len(), 1, "fill in flight");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled(), "the read was cancelled mid-fill");
        assert!(reader.cache.fills.lock().unwrap().is_empty(), "no fill lock left behind");
        assert_eq!(reader.cache.disk_reserved(), 0, "reservation released");
        assert_eq!(fill_dirs(&reader), Vec::<String>::new(), "no fill directory left behind");
        assert_eq!(
            reader.block(&snapshot, 0).await.unwrap().hash,
            fixture.blocks[0].hash,
            "next read fills"
        );
        assert_eq!(disk_entries(&reader), vec!["seg-0"], "the retry published the entry");
    }

    /// Review2 P2 lookup retry, against real `SlateDB` coverage: the view does not cover `H`
    /// when the lookup starts, the first lookup misses on that view, and the view then
    /// advances. The absence is retried once on the covering view instead of being reported.
    /// The first lookup is a stand-in that refreshes the view and reports a miss; the second is
    /// the real index read.
    #[tokio::test]
    async fn absent_entry_before_coverage_is_retried_once_covered() {
        let fixture = ChainFixture::new(8, 2);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = fixture_writer(Arc::clone(&store), &fixture, 4).await;
        writer.step().await.unwrap();
        let gated = GatedStore::new(Arc::clone(&store));
        let gated_store: Arc<dyn ObjectStore> = Arc::new(gated.clone());
        let reader = open_reader(&gated_store, &fixture, CacheConfig::default()).await;
        let mut poll = gated.arm(GateOp::Any, "chain/idx/manifest");
        poll.reached().await;
        writer.step().await.unwrap();
        let before = reader.index.reader.manifest().id();
        let status = tokio::sync::Mutex::new(reader.index.reader.subscribe());
        let poll = std::sync::Mutex::new(poll);
        let calls = AtomicUsize::new(0);
        let hash = fixture.blocks[5].hash;
        let (reader_ref, status, poll, calls) = (&reader, &status, &poll, &calls);
        let found = reader
            .covered_lookup(7, || async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    poll.lock().unwrap().release();
                    let mut status = status.lock().await;
                    status.wait_for(|status| status.current_manifest.id() > before).await.unwrap();
                    return Ok(None);
                }
                reader_ref.index.block_number(hash, 7).await
            })
            .await
            .unwrap();
        assert_eq!(found, Some(5), "the retry on the covering view finds the hash");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly one retry");
    }

    /// Review2 P2 lookup ordering, against real `SlateDB`: the reader's index view advances to
    /// cover `H` while a hash lookup is in flight on the older view. The lookup must not report
    /// the hash unknown just because the view covers `H` by the time coverage is checked.
    #[tokio::test]
    async fn hash_lookup_racing_a_view_refresh_is_not_reported_unknown() {
        let fixture = ChainFixture::new(8, 2);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = fixture_writer(Arc::clone(&store), &fixture, 4).await;
        writer.step().await.unwrap();
        let gated = GatedStore::new(Arc::clone(&store));
        let gated_store: Arc<dyn ObjectStore> = Arc::new(gated.clone());
        let reader = open_reader(&gated_store, &fixture, CacheConfig::default()).await;
        let mut poll = gated.arm(GateOp::Any, "chain/idx/manifest");
        // Hold the reader's manifest poll so its view stays at segment 0.
        poll.reached().await;
        writer.step().await.unwrap();
        let snapshot = reader.snapshot().await.unwrap();
        assert_eq!(snapshot.h, Some(7), "catalog H covers segment 1");
        let before = reader.index.reader.manifest().id();
        let mut status = reader.index.reader.subscribe();
        let mut read = gated.arm(GateOp::Get, "chain/idx/compacted");
        let lookup = reader.block_number(&snapshot, fixture.blocks[5].hash);
        let refresh = async {
            // The lookup's first index read is in flight on the segment-0 view.
            read.reached().await;
            poll.release();
            status.wait_for(|status| status.current_manifest.id() > before).await.unwrap();
            read.release();
        };
        let (found, ()) = tokio::join!(lookup, refresh);
        assert_eq!(found.unwrap(), Some(5), "the hash indexed by segment 1 resolves");
    }
}
