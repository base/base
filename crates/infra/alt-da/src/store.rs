use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use rand::RngCore;
use tokio::io::AsyncReadExt;
use url::Url;

use crate::{
    MAX_OBJECT_BYTES,
    commitment::object_key,
    error::{ConfigError, StoreError},
};

/// Object store keyed by encoded commitment bytes.
#[async_trait]
pub trait Store: Send + Sync {
    /// Fetch preimage bytes for `key`.
    async fn get(&self, key: &[u8]) -> Result<Vec<u8>, StoreError>;
    /// Store `value` at `key`.
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), StoreError>;
}

/// Shared store handle.
pub type DynStore = Arc<dyn Store>;

/// Opens a [`DynStore`] from a backing URL.
#[derive(Debug)]
pub struct StoreOpener;

impl StoreOpener {
    /// Open a store from `s3://bucket/prefix` or `file:///path`.
    pub async fn open(da_url: &str) -> Result<DynStore, ConfigError> {
        let url = Url::parse(da_url).map_err(|err| ConfigError::InvalidUrl(err.to_string()))?;
        match url.scheme() {
            "file" => {
                let path = PathBuf::from(url.path());
                tokio::fs::create_dir_all(&path).await?;
                Ok(Arc::new(FileStore::new(path)))
            }
            "s3" => {
                let bucket = url
                    .host_str()
                    .ok_or_else(|| ConfigError::InvalidUrl("s3 url missing bucket host".into()))?;
                let prefix = url.path().to_string();
                let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
                let client = aws_sdk_s3::Client::new(&config);
                Ok(Arc::new(S3Store::new(client, bucket.to_string(), prefix)))
            }
            scheme => Err(ConfigError::UnsupportedScheme { scheme: scheme.to_string() }),
        }
    }
}

/// Local filesystem object store.
#[derive(Debug)]
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    /// Store objects under `root`, keyed by base64url(commitment).
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Store for FileStore {
    async fn get(&self, key: &[u8]) -> Result<Vec<u8>, StoreError> {
        let path = self.root.join(crate::commitment::object_name(key));
        let meta = match tokio::fs::metadata(&path).await {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound);
            }
            Err(err) => return Err(StoreError::Io(err)),
        };
        if meta.len() > MAX_OBJECT_BYTES as u64 {
            return Err(StoreError::ObjectTooLarge { size: meta.len(), max: MAX_OBJECT_BYTES });
        }
        tokio::fs::read(&path).await.map_err(StoreError::Io)
    }

    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), StoreError> {
        if value.len() > MAX_OBJECT_BYTES {
            return Err(StoreError::ObjectTooLarge {
                size: value.len() as u64,
                max: MAX_OBJECT_BYTES,
            });
        }
        let path = self.root.join(crate::commitment::object_name(key));
        let mut suffix = [0u8; 8];
        rand::rng().fill_bytes(&mut suffix);
        let tmp = path.with_extension(format!("tmp.{}", hex::encode(suffix)));
        tokio::fs::write(&tmp, value).await?;
        if let Err(err) = tokio::fs::rename(&tmp, &path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(StoreError::Io(err));
        }
        Ok(())
    }
}

/// S3 object store.
pub struct S3Store {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

impl S3Store {
    /// Store objects in `bucket` under `prefix`, keyed by base64url(commitment).
    pub const fn new(client: aws_sdk_s3::Client, bucket: String, prefix: String) -> Self {
        Self { client, bucket, prefix }
    }
}

impl std::fmt::Debug for S3Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Store")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Store for S3Store {
    async fn get(&self, key: &[u8]) -> Result<Vec<u8>, StoreError> {
        let object_key = object_key(&self.prefix, key);
        let response = self.client.get_object().bucket(&self.bucket).key(object_key).send().await?;
        if let Some(content_length) = response.content_length()
            && content_length > MAX_OBJECT_BYTES as i64
        {
            return Err(StoreError::ObjectTooLarge {
                size: content_length as u64,
                max: MAX_OBJECT_BYTES,
            });
        }
        read_limited(response.body, MAX_OBJECT_BYTES).await
    }

    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), StoreError> {
        if value.len() > MAX_OBJECT_BYTES {
            return Err(StoreError::ObjectTooLarge {
                size: value.len() as u64,
                max: MAX_OBJECT_BYTES,
            });
        }
        let object_key = object_key(&self.prefix, key);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key)
            .body(ByteStream::from(value.to_vec()))
            .send()
            .await?;
        Ok(())
    }
}

async fn read_limited(body: ByteStream, max: usize) -> Result<Vec<u8>, StoreError> {
    let mut reader = body.into_async_read();
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut chunk).await.map_err(|err| StoreError::S3(err.to_string()))?;
        if n == 0 {
            break;
        }
        if out.len() + n > max {
            return Err(StoreError::ObjectTooLarge { size: (out.len() + n) as u64, max });
        }
        out.extend_from_slice(&chunk[..n]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitment::generate_generic_commitment;

    #[tokio::test]
    async fn file_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().display());
        let store = StoreOpener::open(&url).await.unwrap();
        let key = generate_generic_commitment();
        let value = b"batch-bytes";
        store.put(&key, value).await.unwrap();
        let got = store.get(&key).await.unwrap();
        assert_eq!(got, value);
    }
}
