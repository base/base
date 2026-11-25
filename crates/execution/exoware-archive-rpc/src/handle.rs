//! Lazily opened archive reader shared by the RPC boundary.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use base_exoware_archive::{
    ArchiveError, ArchiveMetrics, ArchiveReader, BlockVerifier, ExowareStore, ReaderLimits,
};
use tokio::sync::OnceCell;
use tracing::warn;

use crate::{ArchiveProvider, ProviderSource};

/// The verified reader over the node's archive namespace.
pub type NodeArchiveReader<P> = ArchiveReader<ExowareStore, ProviderSource<P>>;

/// Result of an archive lookup addressed by a hash the node could not resolve.
#[derive(Debug)]
pub enum HashLookup<T> {
    /// The archive holds the answer.
    Found(T),
    /// The archive verifiably does not hold the hash.
    Miss,
    /// The store failed before the hash resolved: the caller keeps the node's answer.
    Unresolved(ArchiveError),
    /// The hash resolved (or the store returned bad data) and the answer then failed: 4444.
    Failed(ArchiveError),
}

/// Opens the reader on first use and reuses it. Opening reads the config row, so it fails
/// while the store is unreachable or the archive is not provisioned; archive-bound requests
/// then fail fast for `retry_after` before the next attempt.
#[derive(Debug)]
pub struct ArchiveHandle<P> {
    /// Store namespace.
    pub store: Arc<ExowareStore>,
    /// Node state for canonical hashes and state at `H`.
    pub node: Arc<ProviderSource<P>>,
    /// Root and sender checks.
    pub verifier: BlockVerifier,
    /// Node chain id, required to match the config row.
    pub chain_id: u64,
    /// Node genesis hash, required to match the config row.
    pub genesis_hash: B256,
    /// Reader limits.
    pub limits: ReaderLimits,
    /// Budget for all store reads of one RPC request.
    pub budget: Duration,
    /// Delay before retrying a failed open.
    pub retry_after: Duration,
    /// The reader once opened.
    pub reader: OnceCell<NodeArchiveReader<P>>,
    /// When opening last failed.
    pub open_failed_at: Mutex<Option<Instant>>,
}

impl<P: ArchiveProvider> ArchiveHandle<P> {
    /// Returns the reader, opening it if needed.
    pub async fn reader(&self) -> Result<&NodeArchiveReader<P>, ArchiveError> {
        if let Some(reader) = self.reader.get() {
            return Ok(reader);
        }
        if let Some(at) = *self.open_failed_at.lock().unwrap_or_else(PoisonError::into_inner)
            && at.elapsed() < self.retry_after
        {
            return Err(ArchiveError::store("open", "archive reader failed to open recently"));
        }
        let opened = self
            .reader
            .get_or_try_init(|| async {
                tokio::time::timeout(
                    self.budget,
                    ArchiveReader::open(
                        base_exoware_archive::ExowareArchive::new(Arc::clone(&self.store)),
                        self.verifier.clone(),
                        Arc::clone(&self.node),
                        self.chain_id,
                        self.genesis_hash,
                        self.limits,
                    ),
                )
                .await
                .map_err(|_| ArchiveError::store("open", "timed out"))?
            })
            .await;
        if let Err(err) = &opened {
            warn!(error = %err, "archive reader unavailable; serving local data");
            *self.open_failed_at.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(Instant::now());
        }
        opened
    }

    /// Runs one archive request (`fut` opens the reader, begins, reads and concludes) within
    /// the per-request store budget.
    pub async fn bounded<T>(
        &self,
        fut: impl Future<Output = Result<T, ArchiveError>>,
    ) -> Result<T, ArchiveError> {
        tokio::time::timeout(self.budget, fut).await.map_err(|_| {
            ArchiveError::store("request", format!("budget of {:?} exceeded", self.budget))
        })?
    }

    /// Whether `err` is a store outage rather than bad data or a retention change.
    pub const fn is_transient(err: &ArchiveError) -> bool {
        matches!(
            err,
            ArchiveError::Store { .. }
                | ArchiveError::Local { .. }
                | ArchiveError::ConfigMissing
                | ArchiveError::LinkDisabled { .. }
        )
    }

    /// Classifies the outcome of a hash-addressed lookup; `resolved` says whether the hash
    /// mapped to a block before the outcome was decided.
    pub fn classify<T>(outcome: Result<Option<T>, ArchiveError>, resolved: bool) -> HashLookup<T> {
        match outcome {
            Ok(Some(found)) => HashLookup::Found(found),
            Ok(None) => HashLookup::Miss,
            Err(err) if !resolved && Self::is_transient(&err) => {
                ArchiveMetrics::hash_fallbacks().increment(1);
                warn!(error = %err, "archive unavailable for a hash lookup; serving the node's answer");
                HashLookup::Unresolved(err)
            }
            Err(err) => HashLookup::Failed(err),
        }
    }
}
