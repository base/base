use std::time::Duration;

use aws_config::SdkConfig;
use aws_sdk_s3::{Client, config::timeout::TimeoutConfig};

use crate::StoreError;

/// S3 bucket and key prefix using the workspace AWS SDK, as in the snapshotter.
#[derive(Debug, Clone)]
pub struct ReplayS3 {
    /// Configured S3-compatible client.
    pub client: Client,
    /// Existing bucket, managed separately from replay storage.
    pub bucket: String,
    /// Key prefix without leading or trailing slashes.
    pub prefix: String,
}

impl ReplayS3 {
    /// Parse `s3://bucket/prefix` using endpoint, region and credentials from SDK config.
    /// Standard AWS environment credentials are supported by `aws_config::defaults`.
    pub fn new(url: &str, config: &SdkConfig) -> Result<Self, StoreError> {
        let url = url::Url::parse(url).map_err(|_| StoreError::Invalid("S3 URL"))?;
        if url.scheme() != "s3"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
        {
            return Err(StoreError::Invalid("expected s3://bucket/prefix"));
        }
        let bucket = url.host_str().ok_or(StoreError::Invalid("S3 bucket"))?.to_owned();
        let client = Client::from_conf(
            aws_sdk_s3::config::Builder::from(config)
                .force_path_style(true)
                .timeout_config(
                    TimeoutConfig::builder()
                        .connect_timeout(Duration::from_secs(10))
                        .read_timeout(Duration::from_secs(30))
                        .operation_timeout(Duration::from_secs(60))
                        .build(),
                )
                .build(),
        );
        Ok(Self { client, bucket, prefix: url.path().trim_matches('/').into() })
    }
}

#[cfg(test)]
mod tests {
    use aws_sdk_s3::config::Credentials;
    use base_replay::ReplayRecord;
    use futures::TryStreamExt;
    use testcontainers::{
        ImageExt,
        core::{IntoContainerPort, Mount},
        runners::AsyncRunner,
    };
    use testcontainers_modules::minio::MinIO;

    use crate::{ReplayS3, ReplayStore, ReplayWriter};

    #[tokio::test]
    async fn minio_round_trip_ranges_and_create_only() {
        let access_key = uuid::Uuid::new_v4().simple().to_string();
        let secret_key = uuid::Uuid::new_v4().simple().to_string();
        let server = MinIO::default()
            .with_name("cgr.dev/chainguard/minio")
            .with_env_var("MINIO_ROOT_USER", &access_key)
            .with_env_var("MINIO_ROOT_PASSWORD", &secret_key)
            .with_tag(
                "latest@sha256:bd014394a80898e68c149f2311fdf8d5a2c2f3bb2c33b9327ae6d02b4b065ae1",
            )
            .with_mount(Mount::tmpfs_mount("/data"))
            .with_mapped_port(0, 9000.tcp())
            .start()
            .await
            .unwrap();
        let endpoint =
            format!("http://127.0.0.1:{}", server.get_host_port_ipv4(9000).await.unwrap());
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region("us-east-1")
            .endpoint_url(endpoint)
            .credentials_provider(Credentials::new(access_key, secret_key, None, None, "test"))
            .load()
            .await;
        let backend = ReplayS3::new("s3://replay-test/capture", &config).unwrap();
        backend.client.create_bucket().bucket("replay-test").send().await.unwrap();
        let store = ReplayStore::S3(backend);
        let mut writer = ReplayWriter::new(store.clone(), "base".into()).unwrap();
        let params = r#"[ { "n": -0.10e+02, "s": "\u0061" } ]"#;
        for ms in [59_999, 1, 60_000, 180_000] {
            let record: ReplayRecord = serde_json::from_str(&format!(r#"{{"stream":"builder_ingress","received_at_ms":{ms},"method":"call","params":{params},"source":"x"}}"#)).unwrap();
            writer.push(&record).await.unwrap();
        }
        writer.close().await.unwrap();
        let records: Vec<_> = store.read("base", 59_999..=180_000).try_collect().await.unwrap();
        assert_eq!(
            records.iter().map(|r| r.received_at_ms).collect::<Vec<_>>(),
            [59_999, 60_000, 180_000]
        );
        assert!(records.iter().all(|r| r.params.get().as_bytes() == params.as_bytes()));
        let mut collision = ReplayWriter::new(store.clone(), "base".into()).unwrap();
        collision.push(&records[0]).await.unwrap();
        assert!(collision.close().await.is_err());
        let original: Vec<_> = store.read("base", 0..=59_999).try_collect().await.unwrap();
        assert_eq!(original.iter().map(|r| r.received_at_ms).collect::<Vec<_>>(), [1, 59_999]);
        let ReplayStore::S3(backend) = &store else { panic!("S3 store expected") };
        let keys = backend.client.list_objects_v2().bucket("replay-test").send().await.unwrap();
        assert_eq!(
            keys.contents().iter().map(|o| o.key().unwrap()).collect::<Vec<_>>(),
            [
                "capture/base/1970/01/01/00/00.jsonl.zst",
                "capture/base/1970/01/01/00/01.jsonl.zst",
                "capture/base/1970/01/01/00/03.jsonl.zst"
            ]
        );
    }
}
