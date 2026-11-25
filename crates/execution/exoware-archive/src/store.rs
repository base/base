//! Store access: a small trait over the Exoware SDK so tests can inject write faults.

use std::{fmt, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use exoware_sdk::{
    PrefixedStoreClient, RangeMode, ReadSession, StoreClient, StoreKeyPrefix, StoreWriteBatch,
    prune_policy::PrunePolicy,
};

use crate::ArchiveError;

/// Default namespace under which every archive key is stored.
pub const DEFAULT_NAMESPACE: &str = "base-archive/";

/// Default per-request timeout.
pub const DEFAULT_STORE_TIMEOUT: Duration = Duration::from_secs(5);

/// Rows per `GetMany` response frame.
pub const GET_MANY_BATCH: u32 = 1_024;

/// Reads that share one monotonic sequence floor: once a read observes a sequence number,
/// later reads in the session see every write sequenced at or before it.
#[async_trait]
pub trait StoreSession: Send + Sync {
    /// Reads one key.
    async fn get(&self, key: &Bytes) -> Result<Option<Bytes>, ArchiveError>;

    /// Reads several keys; the result is in request order.
    async fn get_many(&self, keys: &[Bytes]) -> Result<Vec<Option<Bytes>>, ArchiveError>;

    /// Reads the inclusive range `[start, end]`, fully consuming the stream.
    async fn range(
        &self,
        start: &Bytes,
        end: &Bytes,
        limit: usize,
        mode: RangeMode,
    ) -> Result<Vec<(Bytes, Bytes)>, ArchiveError>;
}

/// The store operations the archive needs. Keys are logical: the implementation applies the
/// namespace.
#[async_trait]
pub trait ArchiveStore: Send + Sync + fmt::Debug + 'static {
    /// Session type.
    type Session: StoreSession;

    /// Opens a monotonic read session.
    fn session(&self) -> Self::Session;

    /// Writes `rows` in one atomic `Put` and returns its sequence number. On error the write
    /// may or may not have been applied.
    async fn put(&self, rows: &[(Bytes, Bytes)]) -> Result<u64, ArchiveError>;

    /// Applies prune policies, in order, to this namespace.
    async fn prune(&self, policies: &[PrunePolicy]) -> Result<(), ArchiveError>;

    /// Length of the physical key prefix added to every key, for request-size accounting.
    fn key_prefix_len(&self) -> usize;
}

/// [`ArchiveStore`] over an Exoware [`PrefixedStoreClient`].
#[derive(Debug, Clone)]
pub struct ExowareStore {
    /// Namespaced client.
    pub client: PrefixedStoreClient,
    /// Per-request timeout.
    pub timeout: Duration,
}

impl ExowareStore {
    /// Creates a store for `url` under `namespace`. The SDK reads the API key from
    /// `EXOWARE_API_KEY` when set.
    pub fn connect(url: &str, namespace: &str, timeout: Duration) -> Result<Self, ArchiveError> {
        let client = StoreClient::builder()
            .url(url)
            .build()
            .map_err(|e| ArchiveError::store("connect", e))?;
        Self::new(client, namespace, timeout)
    }

    /// Wraps an existing client under `namespace`.
    pub fn new(client: StoreClient, namespace: &str, timeout: Duration) -> Result<Self, ArchiveError> {
        let prefix = StoreKeyPrefix::new(Bytes::copy_from_slice(namespace.as_bytes()))
            .map_err(|e| ArchiveError::InvalidConfig { reason: format!("namespace: {e}") })?;
        Ok(Self { client: client.prefixed(prefix), timeout })
    }

    /// Runs `fut` under the store timeout.
    pub async fn bounded<T, E: fmt::Display>(
        timeout: Duration,
        op: &'static str,
        fut: impl Future<Output = Result<T, E>>,
    ) -> Result<T, ArchiveError> {
        match tokio::time::timeout(timeout, fut).await {
            Ok(result) => result.map_err(|e| ArchiveError::store(op, e)),
            Err(_) => Err(ArchiveError::store(op, format!("timed out after {timeout:?}"))),
        }
    }
}

#[async_trait]
impl ArchiveStore for ExowareStore {
    type Session = ExowareSession;

    fn session(&self) -> Self::Session {
        ExowareSession { session: self.client.create_session(), timeout: self.timeout }
    }

    async fn put(&self, rows: &[(Bytes, Bytes)]) -> Result<u64, ArchiveError> {
        let mut batch = StoreWriteBatch::new();
        batch.reserve(rows.len());
        for (key, value) in rows {
            batch.push(&self.client, key, value).map_err(|e| ArchiveError::store("put", e))?;
        }
        Self::bounded(self.timeout, "put", self.client.ingest().put_prepared(&batch)).await
    }

    async fn prune(&self, policies: &[PrunePolicy]) -> Result<(), ArchiveError> {
        Self::bounded(self.timeout, "prune", self.client.prune().prune(policies)).await
    }

    fn key_prefix_len(&self) -> usize {
        self.client.key_prefix().prefix().len()
    }
}

/// [`StoreSession`] over an Exoware monotonic [`ReadSession`].
#[derive(Debug, Clone)]
pub struct ExowareSession {
    /// SDK session.
    pub session: ReadSession,
    /// Per-request timeout.
    pub timeout: Duration,
}

#[async_trait]
impl StoreSession for ExowareSession {
    async fn get(&self, key: &Bytes) -> Result<Option<Bytes>, ArchiveError> {
        ExowareStore::bounded(self.timeout, "get", self.session.get(key)).await
    }

    async fn get_many(&self, keys: &[Bytes]) -> Result<Vec<Option<Bytes>>, ArchiveError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let refs: Vec<&Bytes> = keys.iter().collect();
        let found = ExowareStore::bounded(self.timeout, "get_many", async {
            self.session.get_many(&refs, GET_MANY_BATCH).await?.collect().await
        })
        .await?;
        Ok(keys.iter().map(|key| found.get(key).cloned()).collect())
    }

    async fn range(
        &self,
        start: &Bytes,
        end: &Bytes,
        limit: usize,
        mode: RangeMode,
    ) -> Result<Vec<(Bytes, Bytes)>, ArchiveError> {
        ExowareStore::bounded(
            self.timeout,
            "range",
            self.session.range_with_mode(start, end, limit, mode),
        )
        .await
    }
}
