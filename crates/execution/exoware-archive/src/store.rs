//! Store access: a small trait over the Exoware SDK so tests can inject write faults.

use std::{fmt, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use exoware_sdk::{
    PrefixedStoreClient, RangeMode, ReadSession, StoreClient, StoreKeyPrefix, StoreWriteBatch,
    limits::{MAX_RESPONSE_MESSAGE_BYTES, MAX_VALUE_LEN},
    prune_policy::PrunePolicy,
};

use crate::ArchiveError;

/// Default namespace under which every archive key is stored.
pub const DEFAULT_NAMESPACE: &str = "base-archive/";

/// Default per-request timeout.
pub const DEFAULT_STORE_TIMEOUT: Duration = Duration::from_secs(5);

/// Rows per `GetMany` and `Range` response frame. Any row may hold a value of up to
/// [`MAX_VALUE_LEN`], so a frame of this many rows stays within the SDK's
/// [`MAX_RESPONSE_MESSAGE_BYTES`] whatever the values are; streams keep reading frames.
pub const GET_MANY_BATCH: u32 = (MAX_RESPONSE_MESSAGE_BYTES / (MAX_VALUE_LEN + 1024)) as u32;

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
    pub fn new(
        client: StoreClient,
        namespace: &str,
        timeout: Duration,
    ) -> Result<Self, ArchiveError> {
        Self::validate_namespace(namespace)?;
        let prefix = StoreKeyPrefix::new(Bytes::copy_from_slice(namespace.as_bytes()))
            .map_err(|e| ArchiveError::InvalidConfig { reason: format!("namespace: {e}") })?;
        Ok(Self { client: client.prefixed(prefix), timeout })
    }

    /// Requires `namespace` to match `^[a-z0-9-]+/$`: one segment of lowercase letters, digits
    /// and dashes followed by a single trailing slash. No valid namespace is then a byte prefix
    /// of another, so one archive's ranges and prunes never reach a neighbor's keys.
    pub fn validate_namespace(namespace: &str) -> Result<(), ArchiveError> {
        let body = namespace.strip_suffix('/').unwrap_or_default();
        if body.is_empty()
            || !body.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(ArchiveError::InvalidConfig {
                reason: format!("namespace {namespace:?} must match ^[a-z0-9-]+/$"),
            });
        }
        Ok(())
    }

    /// Runs `fut` under the store timeout.
    pub async fn bounded<T, E: fmt::Display>(
        timeout: Duration,
        op: &'static str,
        fut: impl Future<Output = Result<T, E>>,
    ) -> Result<T, ArchiveError> {
        tokio::time::timeout(timeout, fut).await.map_or_else(
            |_| Err(ArchiveError::store(op, format!("timed out after {timeout:?}"))),
            |result| result.map_err(|e| ArchiveError::store(op, e)),
        )
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
        ExowareStore::bounded(self.timeout, "range", async {
            self.session
                .range_stream_with_mode(start, end, limit, GET_MANY_BATCH as usize, mode)
                .await?
                .collect()
                .await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames are bounded by bytes: a worst-case frame of maximum-size values fits the SDK's
    /// response budget. This checks the constant only; that `get_many` and `range` pass it to
    /// the SDK is visible at their call sites (the SDK session is a concrete type with no
    /// recording seam).
    #[test]
    fn frame_rows_fit_the_response_budget() {
        assert_eq!(GET_MANY_BATCH, 15, "15 rows of up to 32 MiB per frame");
        assert!(
            GET_MANY_BATCH as usize * (MAX_VALUE_LEN + 1024) <= MAX_RESPONSE_MESSAGE_BYTES,
            "a worst-case frame fits the response budget"
        );
    }

    /// Single-segment lowercase namespaces with one trailing slash are accepted; anything that
    /// could be a byte prefix of another namespace is rejected.
    #[test]
    fn namespace_validation() {
        for ok in ["base-archive/", "a/", "mainnet-2/", "0/"] {
            assert_eq!(ExowareStore::validate_namespace(ok), Ok(()), "{ok:?} accepted");
            assert!(
                ExowareStore::new(
                    StoreClient::new("http://127.0.0.1:1"),
                    ok,
                    DEFAULT_STORE_TIMEOUT
                )
                .is_ok(),
                "{ok:?} accepted by the constructor"
            );
        }
        for bad in ["", "/", "base-archive", "a/b/", "a//", "A/", "a_b/", "a/x", "a b/", "é/"] {
            assert!(
                matches!(
                    ExowareStore::validate_namespace(bad),
                    Err(ArchiveError::InvalidConfig { .. })
                ),
                "{bad:?} rejected"
            );
            assert!(
                matches!(
                    ExowareStore::new(
                        StoreClient::new("http://127.0.0.1:1"),
                        bad,
                        DEFAULT_STORE_TIMEOUT
                    ),
                    Err(ArchiveError::InvalidConfig { .. })
                ),
                "{bad:?} rejected by the constructor"
            );
        }
    }
}
