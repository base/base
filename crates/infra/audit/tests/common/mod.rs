//! Common test harness for audit integration tests with S3 fixtures.

use testcontainers::{
    ImageExt,
    core::{IntoContainerPort, Mount},
    runners::AsyncRunner,
};
use testcontainers_modules::minio::MinIO;
use uuid::Uuid;

pub(crate) struct TestHarness {
    pub s3_client: aws_sdk_s3::Client,
    pub bucket_name: String,
    _minio_container: testcontainers::ContainerAsync<MinIO>,
}

impl TestHarness {
    pub(crate) async fn new() -> anyhow::Result<Self> {
        // MinIO no longer publishes community images: Docker Hub dropped them and quay.io
        // refuses anonymous pulls. Run Chainguard's build of MinIO RELEASE.2026-09-22T19-25-18Z,
        // pinned by digest (`docker buildx imagetools inspect cgr.dev/chainguard/minio:latest`
        // prints the current one). Unlike MinIO's image it declares no volume and exposes no
        // port, so give `/data` a tmpfs (MinIO cannot rename directories across overlay layers)
        // and publish the S3 port.
        let minio_container = MinIO::default()
            .with_name("cgr.dev/chainguard/minio")
            .with_tag(
                "latest@sha256:bd014394a80898e68c149f2311fdf8d5a2c2f3bb2c33b9327ae6d02b4b065ae1",
            )
            .with_mount(Mount::tmpfs_mount("/data"))
            .with_mapped_port(0, 9000.tcp())
            .start()
            .await?;
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

        let s3_client = aws_sdk_s3::Client::new(&config);
        let bucket_name =
            format!("test-bucket-{}", Uuid::new_v5(&Uuid::NAMESPACE_OID, s3_endpoint.as_bytes()));

        s3_client.create_bucket().bucket(&bucket_name).send().await?;

        Ok(Self { s3_client, bucket_name, _minio_container: minio_container })
    }
}
