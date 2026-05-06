//! Common test harness for snapshotter integration tests with `MinIO`.

use anyhow::Result;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::minio::MinIO;

pub(crate) struct TestHarness {
    pub storage_client: aws_sdk_s3::Client,
    pub bucket_name: String,
    _minio_container: testcontainers::ContainerAsync<MinIO>,
}

impl TestHarness {
    pub(crate) async fn new() -> Result<Self> {
        let minio_container = MinIO::default().start().await?;
        let s3_port = minio_container.get_host_port_ipv4(9000).await?;
        let s3_endpoint = format!("http://127.0.0.1:{s3_port}");

        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region("us-east-1")
            .endpoint_url(&s3_endpoint)
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "minioadmin",
                "minioadmin",
                None,
                None,
                "test",
            ))
            .load()
            .await;

        let s3_client = aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::config::Builder::from(&config).force_path_style(true).build(),
        );

        let bucket_name = format!("test-snapshots-{}", std::process::id());

        s3_client.create_bucket().bucket(&bucket_name).send().await?;

        Ok(Self { storage_client: s3_client, bucket_name, _minio_container: minio_container })
    }
}
