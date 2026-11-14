//! Node sources the writer pulls finalized ranges from, tried round-robin.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use jsonrpsee::{
    core::{
        client::{ClientT, Error as ClientError},
        http_helpers::HttpError,
        middleware::layer::RpcLogger,
    },
    http_client::{
        HttpClient, HttpClientBuilder, RpcService,
        transport::{self, HttpBackend},
    },
    types::error::OVERSIZED_RESPONSE_CODE,
};
use reth_rpc_layer::{AuthClientLayer, AuthClientService, JwtSecret};
use tower::ServiceBuilder;
use tracing::{debug, warn};

use crate::{
    ArchiveApiClient, ArchiveBlock, ArchiveReader, ArchiveRpcError, ArchiveStatus, BlockData,
    ChainTip, ChunkVerifier, HistoryStoreError,
};

/// Default maximum JSON-RPC response size for range calls (512 `MiB`). Hex JSON roughly doubles
/// the payload, so this admits a full 64 `MiB` default range with headroom; larger responses are
/// reported as [`HistoryStoreError::SourceRangeTooLarge`] and the writer halves its chunk.
pub const DEFAULT_MAX_RESPONSE_BYTES: u32 = 512 * 1024 * 1024;

/// What the reachable sources reported in one status poll.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStatus {
    /// Highest finalized block any source reports.
    pub finalized: Option<u64>,
    /// Smallest `maxRangeBlocks` any source advertises; a chunk at or below it is accepted by
    /// every source in the rotation.
    pub max_range_blocks: Option<u64>,
}

/// One node that serves the `archive_` namespace.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ChainSource: Send + Sync {
    /// Human-readable name for logs.
    fn name(&self) -> String;

    /// Calls `archive_status`.
    async fn status(&self) -> Result<ArchiveStatus, HistoryStoreError>;

    /// Calls `archive_getBlockRange(from, to)`.
    async fn get_block_range(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<ArchiveBlock>, HistoryStoreError>;
}

/// HTTP client that signs every request with a JWT, for nodes serving `archive_` on their
/// authenticated (engine) RPC port.
pub type AuthHttpClient = HttpClient<RpcLogger<RpcService<AuthClientService<HttpBackend>>>>;

/// A node reached over HTTP JSON-RPC.
#[derive(Debug, Clone)]
pub struct RpcChainSource<C = HttpClient> {
    /// Endpoint URL.
    pub url: String,
    /// HTTP client.
    pub client: C,
}

impl RpcChainSource {
    /// Creates an unauthenticated source for `url` with the given request timeout.
    pub fn new(url: impl Into<String>, timeout: Duration) -> Result<Self, HistoryStoreError> {
        let url = url.into();
        let client = Self::client_builder(timeout)
            .build(&url)
            .map_err(|err| HistoryStoreError::Source(err.to_string()))?;
        Ok(Self { url, client })
    }

    /// Client settings shared by both transports: request timeout and response size cap.
    pub fn client_builder(timeout: Duration) -> HttpClientBuilder {
        HttpClientBuilder::default()
            .request_timeout(timeout)
            .max_response_size(DEFAULT_MAX_RESPONSE_BYTES)
    }
}

impl RpcChainSource<AuthHttpClient> {
    /// Creates a source for a node's authenticated RPC port (`--authrpc.port`), signing each
    /// request with `secret` (the node's `--authrpc.jwtsecret`).
    pub fn with_jwt(
        url: impl Into<String>,
        timeout: Duration,
        secret: JwtSecret,
    ) -> Result<Self, HistoryStoreError> {
        let url = url.into();
        let client = RpcChainSource::client_builder(timeout)
            .set_http_middleware(ServiceBuilder::new().layer(AuthClientLayer::new(secret)))
            .build(&url)
            .map_err(|err| HistoryStoreError::Source(err.to_string()))?;
        Ok(Self { url, client })
    }
}

#[async_trait]
impl<C: ClientT + Send + Sync> ChainSource for RpcChainSource<C> {
    fn name(&self) -> String {
        self.url.clone()
    }

    async fn status(&self) -> Result<ArchiveStatus, HistoryStoreError> {
        ArchiveApiClient::status(&self.client)
            .await
            .map_err(|err| HistoryStoreError::Source(format!("{}: {err}", self.url)))
    }

    async fn get_block_range(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<ArchiveBlock>, HistoryStoreError> {
        ArchiveApiClient::get_block_range(&self.client, from, to)
            .await
            .map_err(|err| self.range_error(from, to, err))
    }
}

impl<C> RpcChainSource<C> {
    /// Maps a range-call failure: block or byte cap rejections (`4003`, `4004`, the server's
    /// `-32008` oversized response) and a response over this client's size limit become
    /// [`HistoryStoreError::SourceRangeTooLarge`], the busy rejection (`4008`) becomes
    /// [`HistoryStoreError::SourceBusy`], and everything else is a plain source error.
    pub fn range_error(&self, from: u64, to: u64, err: ClientError) -> HistoryStoreError {
        if matches!(&err, ClientError::Call(obj) if obj.code() == ArchiveRpcError::BUSY) {
            return HistoryStoreError::SourceBusy {
                source_name: self.url.clone(),
                reason: err.to_string(),
            };
        }
        let too_large = match &err {
            ClientError::Call(obj) => matches!(
                obj.code(),
                ArchiveRpcError::TOO_MANY_BLOCKS
                    | ArchiveRpcError::TOO_MANY_BYTES
                    | OVERSIZED_RESPONSE_CODE
            ),
            ClientError::Transport(inner) => matches!(
                inner.downcast_ref::<transport::Error>(),
                Some(transport::Error::Http(HttpError::TooLarge))
            ),
            _ => false,
        };
        if too_large {
            return HistoryStoreError::SourceRangeTooLarge {
                source_name: self.url.clone(),
                from,
                to,
                reason: err.to_string(),
            };
        }
        HistoryStoreError::Source(format!("{}: {err}", self.url))
    }
}

/// The writer's node list, tried round-robin.
pub struct SourcePool {
    /// Configured sources.
    pub sources: Vec<Arc<dyn ChainSource>>,
    /// Index of the source to try first on the next call.
    pub next: AtomicUsize,
}

impl fmt::Debug for SourcePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourcePool")
            .field("sources", &self.sources.iter().map(|s| s.name()).collect::<Vec<_>>())
            .finish()
    }
}

impl SourcePool {
    /// Creates a pool.
    pub const fn new(sources: Vec<Arc<dyn ChainSource>>) -> Self {
        Self { sources, next: AtomicUsize::new(0) }
    }

    /// Returns the sources in round-robin order starting at the cursor and advances it.
    pub fn rotation(&self) -> Vec<Arc<dyn ChainSource>> {
        let len = self.sources.len();
        if len == 0 {
            return Vec::new();
        }
        let start = self.next.fetch_add(1, Ordering::Relaxed) % len;
        (0..len).map(|i| Arc::clone(&self.sources[(start + i) % len])).collect()
    }

    /// Polls every source's `archive_status`.
    pub async fn status(&self) -> PoolStatus {
        let mut out = PoolStatus::default();
        for source in &self.sources {
            match source.status().await {
                Ok(status) => {
                    out.finalized = out.finalized.max(Some(status.finalized));
                    out.max_range_blocks = Some(
                        out.max_range_blocks
                            .map_or(status.max_range_blocks, |m| m.min(status.max_range_blocks)),
                    );
                }
                Err(err) => warn!(source = %source.name(), error = %err, "archive status failed"),
            }
        }
        out
    }

    /// Fetches and verifies `from..=to`, moving to the next node when a node is busy, fails or
    /// its payload fails verification. When no node succeeds and at least one rejected the range
    /// as too large, returns that [`HistoryStoreError::SourceRangeTooLarge`] so the caller can
    /// shrink the chunk; when the others were only busy, [`HistoryStoreError::SourceBusy`] so
    /// the caller backs off; otherwise [`HistoryStoreError::NoSource`].
    pub async fn fetch_verified(
        &self,
        from: u64,
        to: u64,
        tip: Option<ChainTip>,
        verifier: &ChunkVerifier,
    ) -> Result<Vec<BlockData>, HistoryStoreError> {
        let mut too_large = None;
        let mut busy = None;
        let mut failed = false;
        for source in self.rotation() {
            let rows = match source.get_block_range(from, to).await {
                Ok(rows) => rows,
                Err(err @ HistoryStoreError::SourceRangeTooLarge { .. }) => {
                    warn!(source = %source.name(), from, to, error = %err, "range too large");
                    too_large = Some(err);
                    continue;
                }
                Err(err @ HistoryStoreError::SourceBusy { .. }) => {
                    debug!(source = %source.name(), from, to, "source busy; trying the next one");
                    busy = Some(err);
                    continue;
                }
                Err(err) => {
                    warn!(source = %source.name(), from, to, error = %err, "range fetch failed");
                    failed = true;
                    continue;
                }
            };
            // Decoding, root checks and sender recovery are CPU work; keep them off the runtime.
            let chunk_verifier = verifier.clone();
            let verified =
                ArchiveReader::blocking(move || Ok(chunk_verifier.verify(from, tip, &rows)))
                    .await?;
            match verified {
                Ok(blocks) if blocks.len() as u64 == to - from + 1 => return Ok(blocks),
                Ok(blocks) => warn!(
                    source = %source.name(),
                    from,
                    to,
                    returned = blocks.len(),
                    "node returned a short range"
                ),
                Err(err) => {
                    warn!(source = %source.name(), from, to, error = %err, "payload rejected");
                }
            }
            failed = true;
        }
        if let Some(err) = too_large {
            return Err(err);
        }
        match busy {
            Some(err) if !failed => Err(err),
            _ => Err(HistoryStoreError::NoSource { from, to }),
        }
    }

    /// Fetches block `number` from the next node in rotation and returns its locally computed
    /// hash (the TDD's confirm fetch). A malformed answer (wrong rows, undecodable header) is
    /// that node's fault, so the next node is tried.
    pub async fn fetch_hash(
        &self,
        number: u64,
    ) -> Result<alloy_primitives::B256, HistoryStoreError> {
        for source in self.rotation() {
            match source.get_block_range(number, number).await {
                Ok(rows) if rows.len() == 1 && rows[0].number == number => match rows[0].decode() {
                    // The row's `number` field is the node's claim; the header is what the hash
                    // commits to, so it must carry the requested number too.
                    Ok(block) if block.header.number == number => return Ok(block.hash),
                    Ok(block) => warn!(
                        source = %source.name(),
                        number,
                        header_number = block.header.number,
                        "confirm fetch returned another block's header"
                    ),
                    Err(err) => {
                        warn!(source = %source.name(), number, error = %err, "confirm fetch malformed");
                    }
                },
                Ok(_) => {
                    warn!(source = %source.name(), number, "confirm fetch returned wrong rows")
                }
                Err(err) => warn!(source = %source.name(), number, error = %err, "confirm failed"),
            }
        }
        Err(HistoryStoreError::NoSource { from: number, to: number })
    }
}

#[cfg(test)]
mod tests {
    use jsonrpsee::types::ErrorObject;

    use super::*;

    /// The export's busy rejection becomes `SourceBusy`, so the pool tries the next node.
    #[test]
    fn busy_rejection_maps_to_source_busy() {
        let source = RpcChainSource::new("http://127.0.0.1:1", Duration::from_secs(1)).unwrap();
        let busy = ErrorObject::from(ArchiveRpcError::Busy { max: 2 });
        let err = source.range_error(0, 3, ClientError::Call(busy));
        assert!(
            matches!(&err, HistoryStoreError::SourceBusy { source_name, .. } if source_name == "http://127.0.0.1:1"),
            "4008 must map to SourceBusy, got {err}"
        );
        let too_many = ErrorObject::from(ArchiveRpcError::TooManyBlocks { blocks: 9, max: 8 });
        let err = source.range_error(0, 8, ClientError::Call(too_many));
        assert!(
            matches!(err, HistoryStoreError::SourceRangeTooLarge { from: 0, to: 8, .. }),
            "4003 still maps to SourceRangeTooLarge, got {err}"
        );
    }

    /// A JWT source builds against the auth port without connecting.
    #[test]
    fn jwt_source_builds() {
        let source = RpcChainSource::with_jwt(
            "http://127.0.0.1:8551",
            Duration::from_secs(1),
            JwtSecret::random(),
        )
        .unwrap();
        assert_eq!(source.name(), "http://127.0.0.1:8551", "name is the url");
    }
}
