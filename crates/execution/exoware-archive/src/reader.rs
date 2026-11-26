//! Verified reads from the archive.
//!
//! Every request opens one monotonic store session and first reads the highest marker in
//! it, so later reads see every row sequenced before that marker. After the request's reads,
//! the marker is read again in the same session; if a prune moved the retention window past
//! a block the request touched, the request fails with [`ArchiveError::Pruned`] instead of
//! returning a partial answer.
//!
//! Block, hash and receipt answers are verified per request. State and log answers detect
//! false positives only; their completeness relies on a non-corrupt store, checked on demand
//! by [`crate::SegmentAuditor`].

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_consensus::{Header, constants::KECCAK_EMPTY};
use alloy_primitives::{Address, B256, Bloom, Bytes as AlloyBytes, Log, U256};
use base_common_consensus::{BaseReceipt, BaseTimeUpdateTx, BaseTxEnvelope};
use bytes::Bytes;
use exoware_sdk::RangeMode;
use roaring::RoaringBitmap;
use tracing::{error, warn};

use crate::{
    AccountInfo, ArchiveConfig, ArchiveError, ArchiveKey, ArchiveMetrics, ArchiveStore, BlockBody,
    BlockCodec, BlockVerifier, ExowareArchive, Family, NodeState, OffsetSet, SegmentMarker,
    StateKey, StoreSession,
};

/// Default maximum address plus topic alternatives in one log query.
pub const DEFAULT_MAX_LOG_ALTERNATIVES: usize = 100;

/// Default maximum candidate blocks in one log query.
pub const DEFAULT_MAX_LOG_CANDIDATES: usize = 10_000;

/// Headers read per range page of a wildcard log scan.
pub const HEADER_PAGE: u64 = 1_024;

/// Candidate blocks whose header, body and receipts a log query reads per `get_many`. Each
/// chunk is decoded, verified and filtered, then dropped, so a query holds at most this many
/// blocks of rows at once whatever its candidate count, and a `max_logs` cap stops reading
/// after the chunk that crossed it.
pub const LOG_BLOCKS_PER_READ: usize = 32;

/// Reader limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderLimits {
    /// Maximum address plus topic alternatives in one log query.
    pub max_log_alternatives: usize,
    /// Maximum candidate blocks in one log query.
    pub max_log_candidates: usize,
}

impl Default for ReaderLimits {
    fn default() -> Self {
        Self {
            max_log_alternatives: DEFAULT_MAX_LOG_ALTERNATIVES,
            max_log_candidates: DEFAULT_MAX_LOG_CANDIDATES,
        }
    }
}

/// The archive's extent as resolved at the start of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveSnapshot {
    /// Highest published segment.
    pub highest_segment: u64,
    /// Its marker.
    pub marker: SegmentMarker,
    /// Earliest served segment.
    pub earliest_segment: u64,
}

/// One read request: a monotonic session, the snapshot resolved in it, and the lowest block
/// the request touched.
#[derive(Debug)]
pub struct ArchiveRequest<Sess> {
    /// Store session shared by every read of the request.
    pub session: Sess,
    /// Archive extent, `None` when no segment is published.
    pub snapshot: Option<ArchiveSnapshot>,
    /// Lowest block read, `u64::MAX` before any read.
    pub lowest_block: AtomicU64,
    /// Header hashes of segments verified by linkage in this request.
    pub segment_hashes: Mutex<HashMap<u64, Arc<Vec<B256>>>>,
}

impl<Sess> ArchiveRequest<Sess> {
    /// Records that the request read block `number`.
    pub fn touch(&self, number: u64) {
        self.lowest_block.fetch_min(number, Ordering::Relaxed);
    }
}

/// A verified block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedBlock {
    /// Block hash.
    pub hash: B256,
    /// Header.
    pub header: Header,
    /// Body.
    pub body: BlockBody,
    /// Receipts.
    pub receipts: Vec<BaseReceipt>,
    /// Senders.
    pub senders: Vec<Address>,
}

/// A verified transaction and its position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedTransaction {
    /// Block number.
    pub block_number: u64,
    /// Block hash.
    pub block_hash: B256,
    /// Index in the block.
    pub index: u32,
    /// Header of the block.
    pub header: Header,
    /// Transaction.
    pub transaction: BaseTxEnvelope,
}

/// A log query over `[from, to]`. An empty address list or topic position is a wildcard.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// First block.
    pub from: u64,
    /// Last block.
    pub to: u64,
    /// Addresses, any of which matches.
    pub addresses: Vec<Address>,
    /// Topic alternatives per position.
    pub topics: [Vec<B256>; 4],
}

impl LogFilter {
    /// Whether `log` matches the filter exactly.
    pub fn matches(&self, log: &Log) -> bool {
        if !self.addresses.is_empty() && !self.addresses.contains(&log.address) {
            return false;
        }
        self.topics.iter().enumerate().all(|(position, alternatives)| {
            alternatives.is_empty()
                || log.topics().get(position).is_some_and(|topic| alternatives.contains(topic))
        })
    }

    /// Whether the filter has no address or topic constraint.
    pub fn is_wildcard(&self) -> bool {
        self.addresses.is_empty() && self.topics.iter().all(Vec::is_empty)
    }

    /// Address plus topic alternatives.
    pub fn alternatives(&self) -> usize {
        self.addresses.len() + self.topics.iter().map(Vec::len).sum::<usize>()
    }
}

/// A matching log with its position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedLog {
    /// Block number.
    pub block_number: u64,
    /// Block hash.
    pub block_hash: B256,
    /// Block timestamp.
    pub block_timestamp: u64,
    /// `BaseTime` millisecond timestamp, when the block carries one.
    pub block_timestamp_ms: Option<u64>,
    /// Transaction hash.
    pub transaction_hash: B256,
    /// Transaction index.
    pub transaction_index: u64,
    /// Log index within the block.
    pub log_index: u64,
    /// The log.
    pub log: Log,
}

/// Verified reader over an archive.
#[derive(Debug)]
pub struct ArchiveReader<S, N: ?Sized> {
    /// Archive handle.
    pub archive: ExowareArchive<S>,
    /// Stored config, checked against the node's identity.
    pub config: ArchiveConfig,
    /// Root and sender checks.
    pub verifier: BlockVerifier,
    /// Node state and canonical hashes.
    pub node: Arc<N>,
    /// Limits.
    pub limits: ReaderLimits,
}

impl<S: ArchiveStore, N: NodeState + ?Sized> ArchiveReader<S, N> {
    /// Opens a reader. The stored config must name the node's chain id and genesis hash;
    /// otherwise the archive link is disabled.
    pub async fn open(
        archive: ExowareArchive<S>,
        verifier: BlockVerifier,
        node: Arc<N>,
        chain_id: u64,
        genesis_hash: B256,
        limits: ReaderLimits,
    ) -> Result<Self, ArchiveError> {
        let session = archive.store.session();
        let config =
            ExowareArchive::<S>::read_config(&session).await?.ok_or(ArchiveError::ConfigMissing)?;
        let mismatch = if config.chain_id != chain_id {
            Some(format!(
                "archive chain id {} differs from node chain id {chain_id}",
                config.chain_id
            ))
        } else if config.genesis_hash != genesis_hash {
            Some(format!(
                "archive genesis {} differs from node genesis {genesis_hash}",
                config.genesis_hash
            ))
        } else {
            None
        };
        if let Some(reason) = mismatch {
            ArchiveMetrics::link_disabled().set(1.0);
            error!(reason = %reason, "archive link disabled");
            return Err(ArchiveError::LinkDisabled { reason });
        }
        ArchiveMetrics::link_disabled().set(0.0);
        Ok(Self { archive, config, verifier, node, limits })
    }

    /// Starts a request: opens a session and resolves the highest marker in it.
    pub async fn begin(&self) -> Result<ArchiveRequest<S::Session>, ArchiveError> {
        ArchiveMetrics::requests().increment(1);
        let session = self.archive.store.session();
        let snapshot =
            ExowareArchive::<S>::highest_marker(&session).await?.map(|highest| ArchiveSnapshot {
                highest_segment: highest.segment,
                marker: highest.marker,
                earliest_segment: self.config.earliest_segment(Some(highest.segment)),
            });
        Ok(ArchiveRequest {
            session,
            snapshot,
            lowest_block: AtomicU64::new(u64::MAX),
            segment_hashes: Mutex::default(),
        })
    }

    /// Ends a request: rereads the highest marker in the request's session and fails with
    /// [`ArchiveError::Pruned`] if the retention window moved past a block the request read.
    /// Records verification faults.
    pub async fn conclude<T>(
        &self,
        request: &ArchiveRequest<S::Session>,
        result: Result<T, ArchiveError>,
    ) -> Result<T, ArchiveError> {
        let lowest = request.lowest_block.load(Ordering::Relaxed);
        if lowest != u64::MAX {
            let highest = ExowareArchive::<S>::highest_marker(&request.session).await?;
            let earliest =
                self.config.first_block(self.config.earliest_segment(highest.map(|h| h.segment)));
            if lowest < earliest {
                return Err(ArchiveError::Pruned { block: lowest, earliest });
            }
        }
        if let Err(ArchiveError::Unavailable { family, reason }) = &result {
            ArchiveMetrics::faults(family.name()).increment(1);
            warn!(family = family.name(), reason = %reason, "archive answer rejected");
        }
        result
    }

    /// Highest archived block of a request, if any.
    pub fn highest_block(&self, request: &ArchiveRequest<S::Session>) -> Option<u64> {
        request.snapshot.map(|s| self.config.last_block(s.highest_segment))
    }

    /// Checks that `number` is inside `[E·N, H]` and records it as touched.
    pub fn check_range(
        &self,
        request: &ArchiveRequest<S::Session>,
        number: u64,
    ) -> Result<ArchiveSnapshot, ArchiveError> {
        let snapshot =
            request.snapshot.ok_or(ArchiveError::NotArchived { block: number, highest: None })?;
        let highest = self.config.last_block(snapshot.highest_segment);
        if number > highest {
            return Err(ArchiveError::NotArchived { block: number, highest: Some(highest) });
        }
        let earliest = self.config.first_block(snapshot.earliest_segment);
        if number < earliest {
            return Err(ArchiveError::Pruned { block: number, earliest });
        }
        request.touch(number);
        Ok(snapshot)
    }

    /// Maps a decode failure of stored data to an integrity fault.
    pub fn integrity(err: ArchiveError) -> ArchiveError {
        match err {
            ArchiveError::Decode { family, reason } => ArchiveError::Unavailable { family, reason },
            other => other,
        }
    }

    /// Fails with [`ArchiveError::Unavailable`] for a missing row inside `[E, H]`.
    pub fn require_row(
        value: Option<Bytes>,
        family: Family,
        number: u64,
    ) -> Result<Bytes, ArchiveError> {
        value.ok_or_else(|| {
            ArchiveError::unavailable(family, format!("row for block {number} is missing"))
        })
    }

    /// Verifies a fetched header for block `number` and returns its hash: against the node's
    /// canonical hash when the node has it, else by linkage within the segment to the marker.
    pub async fn verify_header(
        &self,
        request: &ArchiveRequest<S::Session>,
        number: u64,
        header: &Header,
    ) -> Result<B256, ArchiveError> {
        let hash = header.hash_slow();
        if header.number != number {
            return Err(ArchiveError::unavailable(
                Family::Header,
                format!("row for block {number} holds header {}", header.number),
            ));
        }
        let expected = match self.node.canonical_hash(number).await? {
            Some(canonical) => canonical,
            None => {
                let segment = self.config.segment_of(number);
                let hashes = self.segment_hashes(request, segment).await?;
                hashes[(number - self.config.first_block(segment)) as usize]
            }
        };
        if hash != expected {
            return Err(ArchiveError::unavailable(
                Family::Header,
                format!("header {number} hashes to {hash}, expected {expected}"),
            ));
        }
        Ok(hash)
    }

    /// Header hashes of `segment`, verified by parent linkage up to the segment's marker.
    pub async fn segment_hashes(
        &self,
        request: &ArchiveRequest<S::Session>,
        segment: u64,
    ) -> Result<Arc<Vec<B256>>, ArchiveError> {
        if let Some(hashes) =
            request.segment_hashes.lock().unwrap_or_else(PoisonError::into_inner).get(&segment)
        {
            return Ok(Arc::clone(hashes));
        }
        let marker = match request.snapshot {
            Some(s) if s.highest_segment == segment => s.marker,
            _ => {
                ExowareArchive::<S>::marker(&request.session, segment).await?.ok_or_else(|| {
                    ArchiveError::unavailable(
                        Family::Marker,
                        format!("marker {segment} is missing"),
                    )
                })?
            }
        };
        let first = self.config.first_block(segment);
        let last = self.config.last_block(segment);
        let rows = request
            .session
            .range(
                &ArchiveKey::block(Family::Header, first),
                &ArchiveKey::block(Family::Header, last),
                self.config.blocks_per_segment as usize,
                RangeMode::Forward,
            )
            .await?;
        if rows.len() as u64 != self.config.blocks_per_segment {
            return Err(ArchiveError::unavailable(
                Family::Header,
                format!(
                    "segment {segment} has {} of {} headers",
                    rows.len(),
                    self.config.blocks_per_segment
                ),
            ));
        }
        let headers = rows
            .iter()
            .map(|(_, value)| BlockCodec::decode_header(value))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Self::integrity)?;
        if headers.first().map(|h| h.number) != Some(first) {
            return Err(ArchiveError::unavailable(
                Family::Header,
                format!("segment {segment} starts at the wrong block"),
            ));
        }
        let hashes = Arc::new(BlockVerifier::verify_linkage(&headers, marker.last_hash)?);
        request
            .segment_hashes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(segment, Arc::clone(&hashes));
        Ok(hashes)
    }

    /// Reads and verifies the header of block `number`.
    pub async fn header_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        number: u64,
    ) -> Result<(Header, B256), ArchiveError> {
        self.check_range(request, number)?;
        let raw = request.session.get(&ArchiveKey::block(Family::Header, number)).await?;
        let header = BlockCodec::decode_header(&Self::require_row(raw, Family::Header, number)?)
            .map_err(Self::integrity)?;
        let hash = self.verify_header(request, number, &header).await?;
        Ok((header, hash))
    }

    /// Reads and verifies header and body of block `number`.
    pub async fn body_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        number: u64,
    ) -> Result<(Header, B256, BlockBody), ArchiveError> {
        self.check_range(request, number)?;
        let mut values = request
            .session
            .get_many(&[
                ArchiveKey::block(Family::Header, number),
                ArchiveKey::block(Family::Body, number),
            ])
            .await?
            .into_iter();
        let header = BlockCodec::decode_header(&Self::require_row(
            values.next().flatten(),
            Family::Header,
            number,
        )?)
        .map_err(Self::integrity)?;
        let body = BlockCodec::decode_body(&Self::require_row(
            values.next().flatten(),
            Family::Body,
            number,
        )?)
        .map_err(Self::integrity)?;
        let hash = self.verify_header(request, number, &header).await?;
        self.verifier.verify_body(&header, &body)?;
        Ok((header, hash, body))
    }

    /// In the Regolith window, reads block `number`'s account changeset and checks every
    /// deposit nonce against it ([`BlockVerifier::verify_deposit_nonces`]); the receipt root
    /// does not cover that field.
    pub async fn verify_deposit_nonces_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        header: &Header,
        senders: &[Address],
        receipts: &[BaseReceipt],
    ) -> Result<(), ArchiveError> {
        if !self.verifier.deposit_nonce_uncovered(header.timestamp)
            || !receipts.iter().any(|r| matches!(r, BaseReceipt::Deposit(_)))
        {
            return Ok(());
        }
        let number = header.number;
        let raw = request.session.get(&ArchiveKey::block(Family::AccountChanges, number)).await?;
        let changes = BlockCodec::decode_account_changes(&Self::require_row(
            raw,
            Family::AccountChanges,
            number,
        )?)
        .map_err(Self::integrity)?;
        self.verifier.verify_deposit_nonces(header, senders, receipts, &changes)
    }

    /// Reads and verifies every block family of block `number` except the changesets.
    pub async fn block_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        number: u64,
    ) -> Result<ArchivedBlock, ArchiveError> {
        self.check_range(request, number)?;
        let families = [Family::Header, Family::Body, Family::Receipts, Family::Senders];
        let keys: Vec<Bytes> = families.iter().map(|f| ArchiveKey::block(*f, number)).collect();
        let values = request.session.get_many(&keys).await?;
        let mut rows = families
            .iter()
            .zip(values)
            .map(|(family, value)| Self::require_row(value, *family, number));
        let header = BlockCodec::decode_header(&rows.next().expect("header row")?)
            .map_err(Self::integrity)?;
        let body =
            BlockCodec::decode_body(&rows.next().expect("body row")?).map_err(Self::integrity)?;
        let receipts = BlockCodec::decode_receipts(&rows.next().expect("receipts row")?)
            .map_err(Self::integrity)?;
        let senders = BlockCodec::decode_senders(&rows.next().expect("senders row")?)
            .map_err(Self::integrity)?;
        let hash = self.verify_header(request, number, &header).await?;
        self.verifier.verify_body(&header, &body)?;
        self.verifier.verify_receipts(&header, body.transactions.len(), &receipts)?;
        self.verifier.verify_senders(number, &body.transactions, &senders)?;
        self.verify_deposit_nonces_in(request, &header, &senders, &receipts).await?;
        Ok(ArchivedBlock { hash, header, body, receipts, senders })
    }

    /// Finds the locator row for `hash` in `family` within the snapshot's segment window.
    pub async fn locate(
        &self,
        request: &ArchiveRequest<S::Session>,
        family: Family,
        hash: B256,
    ) -> Result<Option<(u64, Bytes)>, ArchiveError> {
        let Some(snapshot) = request.snapshot else { return Ok(None) };
        let rows = request
            .session
            .range(
                &ArchiveKey::build(family, hash.as_slice(), snapshot.earliest_segment),
                &ArchiveKey::build(family, hash.as_slice(), snapshot.highest_segment),
                1,
                RangeMode::Forward,
            )
            .await?;
        let Some((key, value)) = rows.into_iter().next() else {
            // A prune after `begin` can delete the locator, turning a hit into a miss. Record
            // the whole searched window as touched so `conclude` rejects the miss if retention
            // advanced over it.
            request.touch(self.config.first_block(snapshot.earliest_segment));
            return Ok(None);
        };
        let segment = ArchiveKey::number(&key)
            .filter(|_| ArchiveKey::fields(&key) == Some(hash.as_slice()))
            .ok_or_else(|| {
                ArchiveError::unavailable(family, "locator range returned a foreign key")
            })?;
        Ok(Some((segment, value)))
    }

    /// Decodes the block-hash locator for `hash` without reading the block: the block number
    /// the archive claims, before any header check. A caller that must tell "the archive knows
    /// this hash" from "the store failed first" treats `Some` as resolved.
    pub async fn locate_block_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        hash: B256,
    ) -> Result<Option<u64>, ArchiveError> {
        let Some((segment, value)) = self.locate(request, Family::BlockHash, hash).await? else {
            return Ok(None);
        };
        let number = <[u8; 8]>::try_from(value.as_ref())
            .map(u64::from_be_bytes)
            .map_err(|_| ArchiveError::unavailable(Family::BlockHash, "bad locator value"))?;
        if self.config.segment_of(number) != segment {
            return Err(ArchiveError::unavailable(
                Family::BlockHash,
                "locator segment does not hold its block",
            ));
        }
        Ok(Some(number))
    }

    /// Verifies that block `number` hashes to `hash`, as a located block hash must.
    pub async fn verify_located_block_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        hash: B256,
        number: u64,
    ) -> Result<(), ArchiveError> {
        let (_, found) = self.header_in(request, number).await?;
        if found != hash {
            return Err(ArchiveError::unavailable(
                Family::BlockHash,
                format!("locator for {hash} points at block {number} with hash {found}"),
            ));
        }
        Ok(())
    }

    /// Resolves a block hash to its number, verifying the header hashes to `hash`.
    pub async fn block_number_by_hash_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        hash: B256,
    ) -> Result<Option<u64>, ArchiveError> {
        let Some(number) = self.locate_block_in(request, hash).await? else {
            return Ok(None);
        };
        self.verify_located_block_in(request, hash, number).await?;
        Ok(Some(number))
    }

    /// Resolves a transaction hash, verifying the transaction at the located position hashes
    /// to `hash`.
    pub async fn transaction_by_hash_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        hash: B256,
    ) -> Result<Option<ArchivedTransaction>, ArchiveError> {
        let Some((number, index)) = self.locate_transaction_in(request, hash).await? else {
            return Ok(None);
        };
        self.transaction_at_in(request, hash, number, index).await.map(Some)
    }

    /// Decodes the transaction-hash locator for `hash` without reading the block: the
    /// `(block, index)` the archive claims. `Some` means the archive knows the hash.
    pub async fn locate_transaction_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        hash: B256,
    ) -> Result<Option<(u64, u32)>, ArchiveError> {
        let Some((segment, value)) = self.locate(request, Family::TxHash, hash).await? else {
            return Ok(None);
        };
        if value.len() != 12 {
            return Err(ArchiveError::unavailable(Family::TxHash, "bad locator value"));
        }
        let number = u64::from_be_bytes(value[..8].try_into().expect("8 bytes"));
        let index = u32::from_be_bytes(value[8..].try_into().expect("4 bytes"));
        if self.config.segment_of(number) != segment {
            return Err(ArchiveError::unavailable(
                Family::TxHash,
                "locator segment does not hold its block",
            ));
        }
        Ok(Some((number, index)))
    }

    /// Reads the transaction a locator points at, verifying it hashes to `hash`.
    pub async fn transaction_at_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        hash: B256,
        number: u64,
        index: u32,
    ) -> Result<ArchivedTransaction, ArchiveError> {
        let (header, block_hash, body) = self.body_in(request, number).await?;
        let transaction = body
            .transactions
            .into_iter()
            .nth(index as usize)
            .filter(|tx| tx.tx_hash() == hash)
            .ok_or_else(|| {
                ArchiveError::unavailable(Family::TxHash, format!("locator for {hash} points at block {number} index {index}, which holds another transaction"))
            })?;
        Ok(ArchivedTransaction { block_number: number, block_hash, index, header, transaction })
    }

    /// First block `c` with `block < c ≤ H` where `key` changed. Scans `seg(block)..=seg(H)`;
    /// only `seg(block)` may hold no qualifying offset.
    pub async fn first_change_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        key: &StateKey,
        block: u64,
    ) -> Result<Option<u64>, ArchiveError> {
        let snapshot = self.check_range(request, block)?;
        let highest = self.config.last_block(snapshot.highest_segment);
        let first_segment = self.config.segment_of(block);
        let rows = request
            .session
            .range(
                &ArchiveKey::state(key, first_segment),
                &ArchiveKey::state(key, snapshot.highest_segment),
                2,
                RangeMode::Forward,
            )
            .await?;
        let family = Family::StateHistory;
        for (row_key, value) in rows {
            let segment = ArchiveKey::number(&row_key)
                .filter(|s| (first_segment..=snapshot.highest_segment).contains(s))
                .filter(|_| ArchiveKey::fields(&row_key) == Some(&key.fields()[..]))
                .ok_or_else(|| {
                    ArchiveError::unavailable(family, "state range returned a foreign key")
                })?;
            let set = OffsetSet::decode(family, &value).map_err(Self::integrity)?;
            if set.is_empty() || set.iter().any(|o| u64::from(o) >= self.config.blocks_per_segment)
            {
                return Err(ArchiveError::unavailable(
                    family,
                    format!("invalid posting in segment {segment}"),
                ));
            }
            let first = self.config.first_block(segment);
            if let Some(change) =
                set.iter().map(|o| first + u64::from(o)).find(|c| *c > block && *c <= highest)
            {
                return Ok(Some(change));
            }
            if segment != first_segment {
                return Err(ArchiveError::unavailable(
                    family,
                    format!("posting in segment {segment} has no change after block {block}"),
                ));
            }
        }
        Ok(None)
    }

    /// Requires the node's hash of block `H` to equal the highest marker's `last_hash`, so
    /// that the node's state at `H` continues the archive's history.
    pub async fn require_node_at_highest(
        &self,
        request: &ArchiveRequest<S::Session>,
    ) -> Result<u64, ArchiveError> {
        let snapshot =
            request.snapshot.ok_or(ArchiveError::NotArchived { block: 0, highest: None })?;
        let highest = self.config.last_block(snapshot.highest_segment);
        let node_hash = self.node.canonical_hash(highest).await?;
        if node_hash != Some(snapshot.marker.last_hash) {
            return Err(ArchiveError::unavailable(
                Family::Marker,
                format!(
                    "node hash of block {highest} is {node_hash:?}, marker says {}",
                    snapshot.marker.last_hash
                ),
            ));
        }
        Ok(highest)
    }

    /// Account at the end of block `block`.
    pub async fn account_at_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        address: Address,
        block: u64,
    ) -> Result<Option<AccountInfo>, ArchiveError> {
        let Some(change) =
            self.first_change_in(request, &StateKey::account(address), block).await?
        else {
            let highest = self.require_node_at_highest(request).await?;
            return self.node.account_at(highest, address).await;
        };
        let raw = request.session.get(&ArchiveKey::block(Family::AccountChanges, change)).await?;
        let changes = BlockCodec::decode_account_changes(&Self::require_row(
            raw,
            Family::AccountChanges,
            change,
        )?)
        .map_err(Self::integrity)?;
        let index = changes.binary_search_by_key(&address, |c| c.address).map_err(|_| {
            ArchiveError::unavailable(
                Family::AccountChanges,
                format!(
                    "block {change} is indexed as changing {address} but its changeset lacks it"
                ),
            )
        })?;
        Ok(changes[index].before)
    }

    /// Storage slot at the end of block `block`.
    pub async fn storage_at_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        address: Address,
        slot: B256,
        block: u64,
    ) -> Result<U256, ArchiveError> {
        let key = StateKey::storage(address, slot);
        let Some(change) = self.first_change_in(request, &key, block).await? else {
            let highest = self.require_node_at_highest(request).await?;
            return self.node.storage_at(highest, address, slot).await;
        };
        let raw = request.session.get(&ArchiveKey::block(Family::StorageChanges, change)).await?;
        let changes = BlockCodec::decode_storage_changes(&Self::require_row(
            raw,
            Family::StorageChanges,
            change,
        )?)
        .map_err(Self::integrity)?;
        let index = changes.binary_search_by_key(&(address, slot), |c| (c.address, c.slot)).map_err(|_| {
            ArchiveError::unavailable(
                Family::StorageChanges,
                format!("block {change} is indexed as changing {address} {slot} but its changeset lacks it"),
            )
        })?;
        Ok(changes[index].before)
    }

    /// Code of `address` at the end of block `block`. A missing non-empty bytecode is an
    /// error, never empty code.
    pub async fn code_at_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        address: Address,
        block: u64,
    ) -> Result<AlloyBytes, ArchiveError> {
        let Some(account) = self.account_at_in(request, address, block).await? else {
            return Ok(AlloyBytes::new());
        };
        if account.code_hash == KECCAK_EMPTY || account.code_hash == B256::ZERO {
            return Ok(AlloyBytes::new());
        }
        self.node.bytecode(account.code_hash).await?.ok_or_else(|| ArchiveError::Local {
            reason: format!("bytecode {} of {address} is missing", account.code_hash),
        })
    }

    /// Candidate blocks for `filter`: posting algebra per segment, or header blooms for a
    /// wildcard filter, clamped to `[from, to]`.
    pub async fn log_candidates_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        filter: &LogFilter,
    ) -> Result<Vec<u64>, ArchiveError> {
        if filter.from > filter.to {
            return Err(ArchiveError::LimitExceeded { reason: "from is above to".to_string() });
        }
        self.check_range(request, filter.to)?;
        self.check_range(request, filter.from)?;
        if filter.alternatives() > self.limits.max_log_alternatives {
            return Err(ArchiveError::LimitExceeded {
                reason: format!(
                    "{} address and topic alternatives, limit {}",
                    filter.alternatives(),
                    self.limits.max_log_alternatives
                ),
            });
        }
        let candidates = if filter.is_wildcard() {
            self.bloom_candidates(request, filter).await?
        } else {
            self.posting_candidates(request, filter).await?
        };
        if candidates.len() > self.limits.max_log_candidates {
            return Err(ArchiveError::LimitExceeded {
                reason: format!(
                    "{} candidate blocks, limit {}",
                    candidates.len(),
                    self.limits.max_log_candidates
                ),
            });
        }
        Ok(candidates)
    }

    /// Wildcard candidates: blocks whose header bloom is non-empty. Headers are scanned in
    /// pages of [`HEADER_PAGE`] and the candidate cap is enforced as candidates accumulate, so a
    /// sparse range is not refused for its length alone.
    pub async fn bloom_candidates(
        &self,
        request: &ArchiveRequest<S::Session>,
        filter: &LogFilter,
    ) -> Result<Vec<u64>, ArchiveError> {
        let mut out = Vec::new();
        let mut start = filter.from;
        while start <= filter.to {
            let end = filter.to.min(start.saturating_add(HEADER_PAGE - 1));
            let span = end - start + 1;
            let rows = request
                .session
                .range(
                    &ArchiveKey::block(Family::Header, start),
                    &ArchiveKey::block(Family::Header, end),
                    span as usize,
                    RangeMode::Forward,
                )
                .await?;
            if rows.len() as u64 != span {
                return Err(ArchiveError::unavailable(
                    Family::Header,
                    format!("{} of {span} headers present", rows.len()),
                ));
            }
            for (key, value) in rows {
                let header = BlockCodec::decode_header(&value).map_err(Self::integrity)?;
                if ArchiveKey::number(&key) != Some(header.number) {
                    return Err(ArchiveError::unavailable(
                        Family::Header,
                        "header row holds another block",
                    ));
                }
                if header.logs_bloom != Bloom::ZERO {
                    out.push(header.number);
                    if out.len() > self.limits.max_log_candidates {
                        return Err(ArchiveError::LimitExceeded {
                            reason: format!(
                                "more than {} candidate blocks",
                                self.limits.max_log_candidates
                            ),
                        });
                    }
                }
            }
            let Some(next) = end.checked_add(1) else { break };
            start = next;
        }
        Ok(out)
    }

    /// Reads every posting of one lookup field across segments `[from, to]`.
    pub async fn postings(
        &self,
        request: &ArchiveRequest<S::Session>,
        family: Family,
        fields: &[u8],
        segments: (u64, u64),
    ) -> Result<BTreeMap<u64, RoaringBitmap>, ArchiveError> {
        let rows = request
            .session
            .range(
                &ArchiveKey::build(family, fields, segments.0),
                &ArchiveKey::build(family, fields, segments.1),
                (segments.1 - segments.0 + 1) as usize,
                RangeMode::Forward,
            )
            .await?;
        let mut out = BTreeMap::new();
        for (key, value) in rows {
            let segment = ArchiveKey::number(&key)
                .filter(|s| (segments.0..=segments.1).contains(s))
                .filter(|_| ArchiveKey::fields(&key) == Some(fields))
                .ok_or_else(|| {
                    ArchiveError::unavailable(family, "posting range returned a foreign key")
                })?;
            let set = OffsetSet::decode(family, &value).map_err(Self::integrity)?;
            if set.is_empty() || set.iter().any(|o| u64::from(o) >= self.config.blocks_per_segment)
            {
                return Err(ArchiveError::unavailable(
                    family,
                    format!("invalid posting in segment {segment}"),
                ));
            }
            out.insert(segment, set.bits);
        }
        Ok(out)
    }

    /// Candidates from postings: `OR(addresses) AND OR(topics at p)` for each constrained `p`.
    pub async fn posting_candidates(
        &self,
        request: &ArchiveRequest<S::Session>,
        filter: &LogFilter,
    ) -> Result<Vec<u64>, ArchiveError> {
        let segments = (self.config.segment_of(filter.from), self.config.segment_of(filter.to));
        let mut groups: Vec<BTreeMap<u64, RoaringBitmap>> = Vec::new();
        if !filter.addresses.is_empty() {
            let mut union: BTreeMap<u64, RoaringBitmap> = BTreeMap::new();
            for address in &filter.addresses {
                for (segment, bits) in
                    self.postings(request, Family::LogAddress, address.as_slice(), segments).await?
                {
                    *union.entry(segment).or_default() |= bits;
                }
            }
            groups.push(union);
        }
        for (position, alternatives) in filter.topics.iter().enumerate() {
            if alternatives.is_empty() {
                continue;
            }
            let mut union: BTreeMap<u64, RoaringBitmap> = BTreeMap::new();
            for topic in alternatives {
                let mut fields = [0u8; 33];
                fields[0] = position as u8;
                fields[1..].copy_from_slice(topic.as_slice());
                for (segment, bits) in
                    self.postings(request, Family::LogTopic, &fields, segments).await?
                {
                    *union.entry(segment).or_default() |= bits;
                }
            }
            groups.push(union);
        }
        let mut out = Vec::new();
        let Some((first, rest)) = groups.split_first() else { return Ok(out) };
        for (segment, bits) in first {
            let mut bits = bits.clone();
            for group in rest {
                match group.get(segment) {
                    Some(other) => bits &= other,
                    None => bits.clear(),
                }
            }
            let base = self.config.first_block(*segment);
            out.extend(
                bits.iter()
                    .map(|o| base + u64::from(o))
                    .filter(|n| (filter.from..=filter.to).contains(n)),
            );
        }
        Ok(out)
    }

    /// Logs matching `filter`, exactly filtered on verified receipts. Candidates are read in
    /// chunks of [`LOG_BLOCKS_PER_READ`] blocks; with `max_logs`, a query whose matches exceed
    /// it fails with [`ArchiveError::TooManyLogs`] after the chunk that crossed it, without
    /// reading the rest.
    pub async fn logs_in(
        &self,
        request: &ArchiveRequest<S::Session>,
        filter: &LogFilter,
        max_logs: Option<usize>,
    ) -> Result<Vec<ArchivedLog>, ArchiveError> {
        self.logs_in_chunks(request, filter, max_logs, LOG_BLOCKS_PER_READ).await
    }

    /// [`Self::logs_in`] with `chunk_blocks` candidate blocks per `get_many` (at least one).
    pub async fn logs_in_chunks(
        &self,
        request: &ArchiveRequest<S::Session>,
        filter: &LogFilter,
        max_logs: Option<usize>,
        chunk_blocks: usize,
    ) -> Result<Vec<ArchivedLog>, ArchiveError> {
        let candidates = self.log_candidates_in(request, filter).await?;
        let mut out = Vec::new();
        for chunk in candidates.chunks(chunk_blocks.max(1)) {
            self.chunk_logs(request, filter, chunk, &mut out).await?;
            if let Some(max) = max_logs
                && out.len() > max
            {
                return Err(ArchiveError::TooManyLogs { max, crossing: out[max].block_number });
            }
        }
        Ok(out)
    }

    /// Reads, verifies and filters the candidate blocks `chunk` in one `get_many`, appending
    /// their matching logs to `out`.
    pub async fn chunk_logs(
        &self,
        request: &ArchiveRequest<S::Session>,
        filter: &LogFilter,
        chunk: &[u64],
        out: &mut Vec<ArchivedLog>,
    ) -> Result<(), ArchiveError> {
        let families = [Family::Header, Family::Body, Family::Receipts];
        let keys: Vec<Bytes> = chunk
            .iter()
            .flat_map(|n| families.iter().map(move |f| ArchiveKey::block(*f, *n)))
            .collect();
        let values = request.session.get_many(&keys).await?;
        for (number, rows) in chunk.iter().zip(values.chunks_exact(families.len())) {
            let number = *number;
            let header = BlockCodec::decode_header(&Self::require_row(
                rows[0].clone(),
                Family::Header,
                number,
            )?)
            .map_err(Self::integrity)?;
            let body =
                BlockCodec::decode_body(&Self::require_row(rows[1].clone(), Family::Body, number)?)
                    .map_err(Self::integrity)?;
            let receipts = BlockCodec::decode_receipts(&Self::require_row(
                rows[2].clone(),
                Family::Receipts,
                number,
            )?)
            .map_err(Self::integrity)?;
            let block_hash = self.verify_header(request, number, &header).await?;
            self.verifier.verify_body(&header, &body)?;
            self.verifier.verify_receipts(&header, body.transactions.len(), &receipts)?;
            let block_timestamp_ms = BaseTimeUpdateTx::extract_timestamp_ms(
                &body.transactions,
                number,
                header.timestamp,
            )
            .ok();
            let mut log_index = 0;
            for (tx_index, (tx, receipt)) in body.transactions.iter().zip(&receipts).enumerate() {
                for log in &receipt.as_receipt().logs {
                    if filter.matches(log) {
                        out.push(ArchivedLog {
                            block_number: number,
                            block_hash,
                            block_timestamp: header.timestamp,
                            block_timestamp_ms,
                            transaction_hash: tx.tx_hash(),
                            transaction_index: tx_index as u64,
                            log_index,
                            log: log.clone(),
                        });
                    }
                    log_index += 1;
                }
            }
        }
        Ok(())
    }

    /// Reads and verifies block `number` in its own request.
    pub async fn block(&self, number: u64) -> Result<ArchivedBlock, ArchiveError> {
        let request = self.begin().await?;
        let result = self.block_in(&request, number).await;
        self.conclude(&request, result).await
    }

    /// Reads and verifies the header of block `number` in its own request.
    pub async fn header(&self, number: u64) -> Result<(Header, B256), ArchiveError> {
        let request = self.begin().await?;
        let result = self.header_in(&request, number).await;
        self.conclude(&request, result).await
    }

    /// Resolves a block hash in its own request.
    pub async fn block_number_by_hash(&self, hash: B256) -> Result<Option<u64>, ArchiveError> {
        let request = self.begin().await?;
        let result = self.block_number_by_hash_in(&request, hash).await;
        self.conclude(&request, result).await
    }

    /// Resolves a transaction hash in its own request.
    pub async fn transaction_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<ArchivedTransaction>, ArchiveError> {
        let request = self.begin().await?;
        let result = self.transaction_by_hash_in(&request, hash).await;
        self.conclude(&request, result).await
    }

    /// Account at the end of block `block`, in its own request.
    pub async fn account_at(
        &self,
        address: Address,
        block: u64,
    ) -> Result<Option<AccountInfo>, ArchiveError> {
        let request = self.begin().await?;
        let result = self.account_at_in(&request, address, block).await;
        self.conclude(&request, result).await
    }

    /// Storage slot at the end of block `block`, in its own request.
    pub async fn storage_at(
        &self,
        address: Address,
        slot: B256,
        block: u64,
    ) -> Result<U256, ArchiveError> {
        let request = self.begin().await?;
        let result = self.storage_at_in(&request, address, slot, block).await;
        self.conclude(&request, result).await
    }

    /// Code at the end of block `block`, in its own request.
    pub async fn code_at(&self, address: Address, block: u64) -> Result<AlloyBytes, ArchiveError> {
        let request = self.begin().await?;
        let result = self.code_at_in(&request, address, block).await;
        self.conclude(&request, result).await
    }

    /// Logs matching `filter`, in their own request.
    pub async fn logs(&self, filter: &LogFilter) -> Result<Vec<ArchivedLog>, ArchiveError> {
        let request = self.begin().await?;
        let result = self.logs_in(&request, filter, None).await;
        self.conclude(&request, result).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AuditFinding, BlockCodec, BlockData, ExowareStore, SegmentAuditor,
        test_utils::{
            CONTRACT_CODE, ChainFixture, FaultyStore, FixtureNode, SPARSE_BLOCKS, SPARSE_SLOT,
            TEST_N, TEST_TIMEOUT, TestSimulator, dump, publish_through, publisher, reader,
        },
    };

    /// Runs `fut` under the per-test network budget.
    async fn bounded<F: std::future::Future>(fut: F) -> F::Output {
        tokio::time::timeout(TEST_TIMEOUT, fut).await.expect("test timed out")
    }

    /// Publishes segments 0..=3 (H = 31) of the standard fixture into `namespace`.
    async fn published(
        sim: &TestSimulator,
        namespace: &str,
    ) -> (Arc<ChainFixture>, ExowareArchive<ExowareStore>) {
        let fixture = Arc::new(ChainFixture::standard());
        let config = fixture.config(TEST_N, 0, None);
        let archive = sim.archive(namespace, &config).await;
        publish_through(&archive, &fixture, config, 3).await;
        (fixture, archive)
    }

    /// Test 8: an overwritten receipts row fails the root check and the audit flags its
    /// segment; a tampered header fails linkage when the node no longer has the hash.
    #[tokio::test(flavor = "multi_thread")]
    async fn overwritten_rows_are_detected() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let (fixture, archive) = published(&sim, "t8/").await;
            let foreign = BlockCodec::encode_receipts(&fixture.blocks[11].receipts);
            archive
                .store
                .put(&[(ArchiveKey::block(Family::Receipts, 10), Bytes::from(foreign))])
                .await
                .unwrap();
            let r = reader(archive.clone(), FixtureNode::new(Arc::clone(&fixture))).await;
            assert!(
                matches!(
                    r.block(10).await,
                    Err(ArchiveError::Unavailable { family: Family::Receipts, .. })
                ),
                "overwritten receipts rejected"
            );
            let filter =
                LogFilter { from: 10, to: 10, addresses: vec![], topics: Default::default() };
            assert!(
                matches!(
                    r.logs(&filter).await,
                    Err(ArchiveError::Unavailable { family: Family::Receipts, .. })
                ),
                "logs too"
            );
            let auditor = SegmentAuditor::new(archive.clone(), r.config, fixture.verifier());
            let report = auditor.audit_segment(1).await.unwrap();
            assert_eq!(report.findings.len(), 1, "one finding: {:?}", report.findings);
            assert!(
                matches!(&report.findings[0], AuditFinding::BlockInvalid { block: 10, .. }),
                "block 10 flagged"
            );
            assert!(auditor.audit_segment(0).await.unwrap().is_clean(), "segment 0 is clean");
            assert_eq!(
                auditor.audit_segment(9).await.unwrap().findings,
                vec![AuditFinding::MarkerMissing],
                "unpublished"
            );

            let mut node = FixtureNode::new(Arc::clone(&fixture));
            node.headers_from = 100;
            let r = reader(archive.clone(), node).await;
            assert_eq!(
                r.header(20).await.unwrap().1,
                fixture.blocks[20].hash(),
                "linkage verifies without node hashes"
            );
            let mut header = fixture.blocks[19].header.clone();
            header.extra_data = AlloyBytes::from_static(b"forged");
            archive
                .store
                .put(&[(
                    ArchiveKey::block(Family::Header, 19),
                    Bytes::from(BlockCodec::encode_header(&header)),
                )])
                .await
                .unwrap();
            assert!(
                matches!(
                    r.header(20).await,
                    Err(ArchiveError::Unavailable { family: Family::Header, .. })
                ),
                "forged parent breaks linkage"
            );
        })
        .await;
    }

    /// Test 10 / A6: state reads at a change, before a change at the next segment's offset 0,
    /// before a change at H, and with the next change at H + 1 (delegated to the node).
    #[tokio::test(flavor = "multi_thread")]
    async fn state_reads_follow_first_change() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let (fixture, archive) = published(&sim, "t10/").await;
            assert_eq!(SPARSE_BLOCKS, [8, 12, 31, 32], "cases below assume these change blocks");
            // The node serves state only at H = 31, so a read delegated at the requested
            // block fails instead of returning the oracle's value.
            let node =
                FixtureNode { state_only_at: Some(31), ..FixtureNode::new(Arc::clone(&fixture)) };
            let state_reads = Arc::clone(&node.state_reads);
            let r = reader(archive.clone(), node).await;
            let sparse = fixture.sparse;
            let slot_owner = fixture.contracts[1];
            for (block, case, node_reads) in [
                (7, "change at next segment offset 0", vec![]),
                (8, "change at B", vec![]),
                (12, "next change at H", vec![]),
                (20, "no posting in seg(B), change at H", vec![]),
                (31, "B = H, next change at H + 1: node at H", vec![31, 31]),
            ] {
                let state = &fixture.states[block as usize];
                assert_eq!(
                    r.account_at(sparse, block).await.unwrap(),
                    state.accounts.get(&sparse).copied(),
                    "account: {case}"
                );
                assert_eq!(
                    r.storage_at(slot_owner, SPARSE_SLOT, block).await.unwrap(),
                    state.storage.get(&(slot_owner, SPARSE_SLOT)).copied().unwrap_or_default(),
                    "storage: {case}"
                );
                assert_eq!(
                    std::mem::take(&mut *state_reads.lock().unwrap()),
                    node_reads,
                    "node state reads: {case}"
                );
            }
            assert_ne!(
                fixture.states[20].accounts.get(&sparse),
                fixture.states[31].accounts.get(&sparse),
                "B = 20 must come from the archive, not the node's state at H"
            );
            let request = r.begin().await.unwrap();
            assert_eq!(
                r.first_change_in(&request, &StateKey::account(sparse), 8).await.unwrap(),
                Some(12),
                "first change after 8"
            );
            assert_eq!(
                r.first_change_in(&request, &StateKey::account(sparse), 7).await.unwrap(),
                Some(8),
                "offset 0 of segment 1"
            );
            assert_eq!(
                r.first_change_in(&request, &StateKey::account(sparse), 31).await.unwrap(),
                None,
                "32 is beyond H"
            );
            assert_eq!(
                r.code_at(fixture.contracts[0], 10).await.unwrap(),
                AlloyBytes::from_static(&CONTRACT_CODE),
                "code"
            );
            assert_eq!(
                r.code_at(fixture.recipient, 10).await.unwrap(),
                AlloyBytes::new(),
                "no code"
            );
            assert_eq!(
                r.account_at(sparse, 33).await.unwrap_err(),
                ArchiveError::NotArchived { block: 33, highest: Some(31) },
                "B > H"
            );

            let mut node = FixtureNode::new(Arc::clone(&fixture));
            node.headers_from = 32;
            let r = reader(archive.clone(), node).await;
            assert!(
                matches!(
                    r.account_at(sparse, 31).await,
                    Err(ArchiveError::Unavailable { family: Family::Marker, .. })
                ),
                "node fallback requires the node's hash at H to match the marker"
            );

            let config = fixture.config(TEST_N, 0, Some(2));
            let retained = sim.archive("t10-retain/", &config).await;
            publish_through(&retained, &fixture, config, 3).await;
            let r = reader(retained, FixtureNode::new(Arc::clone(&fixture))).await;
            assert_eq!(
                r.account_at(sparse, 3).await.unwrap_err(),
                ArchiveError::Pruned { block: 3, earliest: 16 },
                "B < E"
            );
        })
        .await;
    }

    /// Test 11: address and topic algebra, a cross-log false positive excluded by the exact
    /// filter, wildcard filters via header blooms, and limits.
    #[tokio::test(flavor = "multi_thread")]
    async fn log_queries() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let (fixture, archive) = published(&sim, "t11/").await;
            let r = reader(archive.clone(), FixtureNode::new(Arc::clone(&fixture))).await;
            let [c0, c1, c2] = fixture.contracts;
            let [e0, _] = fixture.events;
            let filters = [
                LogFilter {
                    from: 1,
                    to: 31,
                    addresses: vec![c0],
                    topics: [vec![e0], vec![], vec![], vec![]],
                },
                LogFilter { from: 3, to: 29, addresses: vec![c1, c2], topics: Default::default() },
                LogFilter {
                    from: 0,
                    to: 31,
                    addresses: vec![],
                    topics: [vec![], vec![B256::from(U256::from(10))], vec![], vec![]],
                },
                LogFilter { from: 0, to: 31, addresses: vec![], topics: Default::default() },
                LogFilter {
                    from: 9,
                    to: 9,
                    addresses: vec![c2],
                    topics: [vec![], vec![B256::from(U256::from(10))], vec![], vec![]],
                },
            ];
            for filter in &filters {
                let got: Vec<_> = r
                    .logs(filter)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|l| (l.block_number, l.log_index, l.log))
                    .collect();
                let expected = fixture.expected_logs(filter);
                assert_eq!(got, expected, "filter {filter:?}");
            }
            assert_eq!(
                fixture.expected_logs(&filters[2]).len(),
                2,
                "topic 1 = 10 matches both logs of block 10"
            );
            assert!(
                fixture.expected_logs(&filters[4]).is_empty(),
                "disjoint filter matches nothing"
            );

            let request = r.begin().await.unwrap();
            let candidates = r.log_candidates_in(&request, &filters[0]).await.unwrap();
            // Posting algebra per block: some log from c0 and some log with topic0 e0, not
            // necessarily the same log.
            let oracle: Vec<u64> = fixture.blocks[1..32]
                .iter()
                .filter(|b| {
                    let logs = || b.receipts.iter().flat_map(|r| r.as_receipt().logs.iter());
                    logs().any(|l| l.address == c0)
                        && logs().any(|l| l.topics().first() == Some(&e0))
                })
                .map(BlockData::number)
                .collect();
            assert_eq!(candidates, oracle, "exact candidates from the fixture's logs");
            for false_positive in [5, 12, 19, 26] {
                assert!(
                    candidates.contains(&false_positive),
                    "block {false_positive} is an index candidate"
                );
                assert!(
                    !fixture
                        .expected_logs(&filters[0])
                        .iter()
                        .any(|(n, _, _)| *n == false_positive),
                    "block {false_positive} has no matching log"
                );
            }
            assert_eq!(
                r.log_candidates_in(&request, &filters[3]).await.unwrap(),
                (1..=31).collect::<Vec<_>>(),
                "wildcard candidates are blocks with a non-empty bloom"
            );
            let logs = r.logs(&filters[2]).await.unwrap();
            assert_eq!(
                logs[0].transaction_hash,
                fixture.blocks[10].body.transactions[1].tx_hash(),
                "tx hash"
            );
            assert_eq!(
                (logs[0].transaction_index, logs[1].transaction_index),
                (1, 2),
                "tx indexes"
            );
            assert_eq!(logs[0].block_hash, fixture.blocks[10].hash(), "block hash");

            let wide = LogFilter {
                addresses: (0..101u8).map(Address::with_last_byte).collect(),
                ..filters[1].clone()
            };
            assert!(
                matches!(r.logs(&wide).await, Err(ArchiveError::LimitExceeded { .. })),
                "101 alternatives"
            );
            let tight = ArchiveReader {
                limits: ReaderLimits { max_log_candidates: 5, ..ReaderLimits::default() },
                ..reader(archive.clone(), FixtureNode::new(Arc::clone(&fixture))).await
            };
            assert!(
                matches!(tight.logs(&filters[3]).await, Err(ArchiveError::LimitExceeded { .. })),
                "candidate limit"
            );
            assert!(
                matches!(tight.logs(&filters[0]).await, Err(ArchiveError::LimitExceeded { .. })),
                "index candidate limit"
            );
            let above = LogFilter { to: 32, ..filters[0].clone() };
            assert_eq!(
                r.logs(&above).await.unwrap_err(),
                ArchiveError::NotArchived { block: 32, highest: Some(31) },
                "to above H"
            );
        })
        .await;
    }

    /// A2 / B9: a request that resolved its snapshot and *successfully* read a block before a
    /// prune has its answer discarded at `conclude`; a request that read after the prune fails
    /// with `Pruned` too; a fresh request is refused up front.
    #[tokio::test(flavor = "multi_thread")]
    async fn paused_reader_across_prune() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, Some(1));
            let archive = sim.archive("t-paused/", &config).await;
            let mut p =
                publisher(archive.clone(), FixtureNode::new(Arc::clone(&fixture)), config).await;
            p.options.prune_grace_segments = 1;
            for segment in 0..2 {
                p.publish_segment(segment).await.unwrap();
            }
            let r = reader(archive.clone(), FixtureNode::new(Arc::clone(&fixture))).await;
            let request = r.begin().await.unwrap();
            assert_eq!(request.snapshot.map(|s| s.earliest_segment), Some(1), "E = 1 at start");
            let answer = r.block_in(&request, 9).await;
            assert_eq!(
                answer.as_ref().map(|b| b.hash),
                Ok(fixture.blocks[9].hash()),
                "read succeeds before the prune"
            );
            let late = r.begin().await.unwrap();
            for segment in 2..4 {
                p.publish_segment(segment).await.unwrap();
            }
            assert_eq!(
                archive.prune(&config, Some(3), 1).await.unwrap(),
                Some(2),
                "segments below 2 pruned"
            );
            assert_eq!(
                r.conclude(&request, answer).await.unwrap_err(),
                ArchiveError::Pruned { block: 9, earliest: 24 },
                "successful answer discarded"
            );
            let result = r.block_in(&late, 9).await;
            assert!(result.is_err(), "rows are gone");
            assert_eq!(
                r.conclude(&late, result).await.unwrap_err(),
                ArchiveError::Pruned { block: 9, earliest: 24 },
                "paused request"
            );
            assert_eq!(
                r.block(9).await.unwrap_err(),
                ArchiveError::Pruned { block: 9, earliest: 24 },
                "fresh request"
            );
            assert_eq!(
                r.block(24).await.unwrap().hash,
                fixture.blocks[24].hash(),
                "retained block"
            );
        })
        .await;
    }

    /// B3: a block or transaction hash whose locator a prune deleted after `begin` is a
    /// `Pruned` error, not a successful miss; an unknown hash in an unpruned window is `None`.
    #[tokio::test(flavor = "multi_thread")]
    async fn locator_miss_after_prune_is_rejected() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, Some(1));
            let archive = sim.archive("t-locator/", &config).await;
            let mut p =
                publisher(archive.clone(), FixtureNode::new(Arc::clone(&fixture)), config).await;
            p.options.prune_grace_segments = 1;
            for segment in 0..2 {
                p.publish_segment(segment).await.unwrap();
            }
            let r = reader(archive.clone(), FixtureNode::new(Arc::clone(&fixture))).await;
            let block_hash = fixture.blocks[9].hash();
            let tx_hash = fixture.blocks[9].body.transactions[1].tx_hash();
            assert_eq!(
                r.block_number_by_hash(block_hash).await,
                Ok(Some(9)),
                "the block hash resolves before the prune"
            );
            assert!(
                r.transaction_by_hash(tx_hash).await.is_ok_and(|tx| tx.is_some()),
                "the transaction hash resolves before the prune"
            );
            let block_request = r.begin().await.unwrap();
            let tx_request = r.begin().await.unwrap();
            let unknown = r.begin().await.unwrap();
            assert_eq!(
                r.conclude(
                    &unknown,
                    r.block_number_by_hash_in(&unknown, B256::repeat_byte(9)).await
                )
                .await,
                Ok(None),
                "unknown hash before any prune is a miss"
            );
            for segment in 2..4 {
                p.publish_segment(segment).await.unwrap();
            }
            assert_eq!(
                archive.prune(&config, Some(3), 1).await.unwrap(),
                Some(2),
                "segments below 2 pruned"
            );
            let rows = dump(archive.store.as_ref()).await;
            for (key, what) in [
                (ArchiveKey::block_hash(block_hash, 1), "block"),
                (ArchiveKey::tx_hash(tx_hash, 1), "transaction"),
            ] {
                assert!(!rows.iter().any(|(k, _)| *k == key), "the {what} locator row is deleted");
            }
            let block = r.block_number_by_hash_in(&block_request, block_hash).await;
            assert_eq!(block, Ok(None), "the locator range finds nothing");
            assert_eq!(
                r.conclude(&block_request, block).await,
                Err(ArchiveError::Pruned { block: 8, earliest: 24 }),
                "block hash miss after prune rejected"
            );
            let tx = r.transaction_by_hash_in(&tx_request, tx_hash).await;
            assert_eq!(tx, Ok(None), "the tx locator range finds nothing");
            assert_eq!(
                r.conclude(&tx_request, tx).await,
                Err(ArchiveError::Pruned { block: 8, earliest: 24 }),
                "tx hash miss after prune rejected"
            );
        })
        .await;
    }

    /// B8: the wildcard candidate cap counts candidates, not scanned headers, so a sparse
    /// range longer than the cap is served while a dense one is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn wildcard_cap_counts_candidates() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let (fixture, archive) = published(&sim, "t-wild/").await;
            let tight = ArchiveReader {
                limits: ReaderLimits { max_log_candidates: 1, ..ReaderLimits::default() },
                ..reader(archive.clone(), FixtureNode::new(Arc::clone(&fixture))).await
            };
            let sparse =
                LogFilter { from: 0, to: 1, addresses: vec![], topics: Default::default() };
            assert_eq!(fixture.blocks[0].header.logs_bloom, Bloom::ZERO, "genesis has no logs");
            let logs = tight.logs(&sparse).await.unwrap();
            assert_eq!(
                logs.len(),
                2,
                "two logs of block 1 over a two-block range with one candidate"
            );
            assert!(logs.iter().all(|l| l.block_number == 1), "both from block 1");
            let dense = LogFilter { from: 1, to: 2, ..sparse };
            assert!(
                matches!(tight.logs(&dense).await, Err(ArchiveError::LimitExceeded { .. })),
                "two candidates exceed one"
            );
        })
        .await;
    }

    /// M2: reading log candidates in chunks of 1, 3 or [`LOG_BLOCKS_PER_READ`] blocks gives
    /// the same logs as the fixture oracle, and each `get_many` holds at most one chunk.
    #[tokio::test(flavor = "multi_thread")]
    async fn chunked_logs_match_the_whole_read() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let (fixture, archive) = published(&sim, "t-chunk/").await;
            let faulty = Arc::new(FaultyStore::new(Arc::clone(&archive.store), vec![]));
            let r = reader(
                ExowareArchive::new(Arc::clone(&faulty)),
                FixtureNode::new(Arc::clone(&fixture)),
            )
            .await;
            let filter =
                LogFilter { from: 0, to: 31, addresses: vec![], topics: Default::default() };
            let expected = fixture.expected_logs(&filter);
            assert!(expected.len() > 3, "the wildcard filter matches logs in many blocks");
            let request = r.begin().await.unwrap();
            let candidates = r.log_candidates_in(&request, &filter).await.unwrap();
            assert_eq!(
                candidates,
                fixture.blocks[1..32]
                    .iter()
                    .filter(|b| b.header.logs_bloom != Bloom::ZERO)
                    .map(BlockData::number)
                    .collect::<Vec<_>>(),
                "wildcard candidates are the fixture's blocks with logs"
            );
            for chunk in [1, 3, LOG_BLOCKS_PER_READ] {
                faulty.take_get_many();
                let logs = r.logs_in_chunks(&request, &filter, None, chunk).await.unwrap();
                let got: Vec<_> =
                    logs.iter().map(|l| (l.block_number, l.log_index, l.log.clone())).collect();
                assert_eq!(got, expected, "chunks of {chunk} blocks give the oracle's logs");
                // Three rows (header, body, receipts) per candidate, one get_many per chunk.
                let sizes: Vec<usize> =
                    candidates.chunks(chunk).map(|blocks| 3 * blocks.len()).collect();
                assert_eq!(
                    faulty.take_get_many(),
                    sizes,
                    "get_many sizes, in order, for chunks of {chunk}"
                );
            }
            let full = r.logs_in(&request, &filter, None).await.unwrap();
            let one = r.logs_in_chunks(&request, &filter, None, 1).await.unwrap();
            assert_eq!(full, one, "default chunking equals block-by-block reads, field for field");
            assert_eq!(r.conclude(&request, Ok(())).await, Ok(()), "request concludes");
        })
        .await;
    }

    /// M2: with `max_logs`, reading stops after the chunk whose logs crossed the cap and
    /// reports the block of the first log past it; a cap the query fits in changes nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn log_cap_stops_after_the_crossing_chunk() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let (fixture, archive) = published(&sim, "t-cap/").await;
            let faulty = Arc::new(FaultyStore::new(Arc::clone(&archive.store), vec![]));
            let r = reader(
                ExowareArchive::new(Arc::clone(&faulty)),
                FixtureNode::new(Arc::clone(&fixture)),
            )
            .await;
            let filter =
                LogFilter { from: 0, to: 31, addresses: vec![], topics: Default::default() };
            let expected = fixture.expected_logs(&filter);
            let request = r.begin().await.unwrap();
            let candidates = r.log_candidates_in(&request, &filter).await.unwrap();
            let chunk = 4;
            let max = 3;
            let crossing = expected[max].0;
            let crossing_chunk =
                candidates.iter().position(|n| *n == crossing).expect("crossing is a candidate")
                    / chunk;
            assert!(
                crossing_chunk + 1 < candidates.len().div_ceil(chunk),
                "chunks remain after the crossing one"
            );
            faulty.take_get_many();
            assert_eq!(
                r.logs_in_chunks(&request, &filter, Some(max), chunk).await,
                Err(ArchiveError::TooManyLogs { max, crossing }),
                "the cap error names the block of log {max}"
            );
            assert_eq!(
                faulty.take_get_many().len(),
                crossing_chunk + 1,
                "no get_many after the crossing chunk"
            );
            let fits =
                r.logs_in_chunks(&request, &filter, Some(expected.len()), chunk).await.unwrap();
            assert_eq!(
                fits.into_iter().map(|l| (l.block_number, l.log_index, l.log)).collect::<Vec<_>>(),
                expected,
                "a cap equal to the match count returns every log"
            );
            assert_eq!(
                faulty.take_get_many().len(),
                candidates.len().div_ceil(chunk),
                "every chunk read under a fitting cap"
            );
            assert_eq!(
                r.logs_in_chunks(&request, &filter, Some(0), chunk).await,
                Err(ArchiveError::TooManyLogs { max: 0, crossing: expected[0].0 }),
                "a zero cap fails at the first matching block"
            );
        })
        .await;
    }

    /// B2: a Regolith-window receipts row whose only change is a deposit nonce passes the
    /// receipt root but is rejected by the reader's changeset check.
    #[tokio::test(flavor = "multi_thread")]
    async fn regolith_nonce_corruption_is_rejected_on_read() {
        bounded(async {
            let sim = TestSimulator::start().await;
            let (fixture, archive) = published(&sim, "t-nonce/").await;
            let r = reader(archive.clone(), FixtureNode::new(Arc::clone(&fixture))).await;
            assert_eq!(
                r.block(5).await.unwrap().receipts,
                fixture.blocks[5].receipts,
                "intact block reads"
            );
            let mut receipts = fixture.blocks[5].receipts.clone();
            match &mut receipts[0] {
                BaseReceipt::Deposit(deposit) => deposit.deposit_nonce = Some(99),
                other => panic!("receipt 0 is a deposit, got {other:?}"),
            }
            archive
                .store
                .put(&[(
                    ArchiveKey::block(Family::Receipts, 5),
                    Bytes::from(BlockCodec::encode_receipts(&receipts)),
                )])
                .await
                .unwrap();
            assert!(
                matches!(
                    r.block(5).await,
                    Err(ArchiveError::Unavailable { family: Family::Receipts, .. })
                ),
                "nonce-only corruption rejected"
            );
        })
        .await;
    }
}
