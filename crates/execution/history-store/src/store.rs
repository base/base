//! Opens the archive's object store from a URL (`--archive.url`).

use std::{sync::Arc, time::Duration};

use object_store::{
    ClientOptions, ObjectStore, RetryConfig,
    aws::{AmazonS3Builder, S3ConditionalPut},
    local::LocalFileSystem,
    memory::InMemory,
};

use crate::{ArchiveLayout, HistoryStoreError};

/// An archive location parsed from a URL.
///
/// - `s3://<bucket>[/<prefix>]`: S3 or an S3-compatible store such as `MinIO`. Credentials and
///   region come from the usual `AWS_*` environment variables; `endpoint` overrides the service
///   URL. Catalog updates use `If-Match` conditional puts.
/// - `file:///<dir>`: a local directory. Readers work; catalog CAS returns
///   [`HistoryStoreError::BackendUnsupported`], so writers need S3 or memory.
/// - `memory://`: an empty in-process store, for tests.
#[derive(Debug, Clone)]
pub struct ArchiveStore {
    /// The object store.
    pub store: Arc<dyn ObjectStore>,
    /// Key layout inside the store.
    pub layout: ArchiveLayout,
}

/// HTTP client settings of an S3 archive.
#[derive(Debug, Clone, Default)]
pub struct StoreOptions {
    /// Connection and request timeouts.
    pub client: ClientOptions,
    /// Retry budget.
    pub retry: RetryConfig,
}

impl StoreOptions {
    /// Connect timeout of node readers.
    pub const READER_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
    /// Retries of one node reader request.
    pub const READER_MAX_RETRIES: usize = 3;
    /// Time after which a node reader stops retrying a request.
    pub const READER_RETRY_TIMEOUT: Duration = Duration::from_secs(15);

    /// Settings for nodes reading the archive inside RPC requests. `object_store` retries a
    /// request up to 10 times over 3 minutes by default, so an unreachable bucket would hold
    /// an RPC call that long; readers give up within [`Self::READER_RETRY_TIMEOUT`] plus one
    /// request instead. The 30 s per-request timeout stays, since a segment fill downloads
    /// whole files in one request.
    pub fn reader() -> Self {
        Self {
            client: ClientOptions::new().with_connect_timeout(Self::READER_CONNECT_TIMEOUT),
            retry: RetryConfig {
                max_retries: Self::READER_MAX_RETRIES,
                retry_timeout: Self::READER_RETRY_TIMEOUT,
                ..RetryConfig::default()
            },
        }
    }
}

impl ArchiveStore {
    /// Opens the store named by `url` with `object_store`'s default client settings, using
    /// `endpoint` as the S3 service URL when set.
    pub fn from_url(url: &str, endpoint: Option<&str>) -> Result<Self, HistoryStoreError> {
        Self::from_url_with(url, endpoint, StoreOptions::default())
    }

    /// Opens the store named by `url`, applying `options` to S3 stores.
    pub fn from_url_with(
        url: &str,
        endpoint: Option<&str>,
        options: StoreOptions,
    ) -> Result<Self, HistoryStoreError> {
        let invalid = |reason: String| HistoryStoreError::InvalidConfig { reason };
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| invalid(format!("archive url {url} has no scheme")))?;
        match scheme {
            "s3" => {
                let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
                if bucket.is_empty() {
                    return Err(invalid(format!("archive url {url} has no bucket")));
                }
                let mut builder = AmazonS3Builder::from_env()
                    .with_bucket_name(bucket)
                    .with_conditional_put(S3ConditionalPut::ETagMatch)
                    .with_client_options(options.client)
                    .with_retry(options.retry);
                if let Some(endpoint) = endpoint {
                    builder = builder
                        .with_endpoint(endpoint)
                        .with_allow_http(endpoint.starts_with("http://"))
                        .with_virtual_hosted_style_request(false);
                }
                let store = builder.build().map_err(|err| invalid(err.to_string()))?;
                Ok(Self { store: Arc::new(store), layout: ArchiveLayout::new(prefix) })
            }
            "file" => {
                std::fs::create_dir_all(rest)?;
                let store = LocalFileSystem::new_with_prefix(rest)
                    .map_err(|err| invalid(err.to_string()))?;
                Ok(Self { store: Arc::new(store), layout: ArchiveLayout::new("") })
            }
            "memory" => {
                Ok(Self { store: Arc::new(InMemory::new()), layout: ArchiveLayout::new(rest) })
            }
            other => Err(invalid(format!("unsupported archive url scheme {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each supported scheme opens, and the S3 prefix lands in the layout.
    #[test]
    fn parses_supported_urls() {
        let s3 = ArchiveStore::from_url("s3://bucket/base/mainnet", Some("http://127.0.0.1:9000"))
            .expect("s3 url with endpoint must open");
        assert_eq!(s3.layout, ArchiveLayout::new("base/mainnet"), "prefix follows the bucket");

        let dir = tempfile::tempdir().expect("tempdir");
        let file = ArchiveStore::from_url(&format!("file://{}", dir.path().display()), None)
            .expect("file url must open");
        assert_eq!(file.layout, ArchiveLayout::new(""), "file stores use the directory as root");

        let memory = ArchiveStore::from_url("memory://chain", None).expect("memory url must open");
        assert_eq!(memory.layout, ArchiveLayout::new("chain"), "memory prefix is kept");
    }

    /// Unknown schemes and missing buckets are configuration errors.
    #[test]
    fn rejects_bad_urls() {
        for url in ["gs://bucket", "s3://", "no-scheme"] {
            let err = ArchiveStore::from_url(url, None).expect_err("url must be rejected");
            assert!(
                matches!(err, HistoryStoreError::InvalidConfig { .. }),
                "{url} must be InvalidConfig, got {err}"
            );
        }
    }
}
