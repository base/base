use rdkafka::{ClientConfig, consumer::StreamConsumer, producer::FutureProducer};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::{kafka, kafka::Kafka, minio::MinIO};
use uuid::Uuid;

pub(crate) struct TestHarness {
    pub s3_client: aws_sdk_s3::Client,
    pub bucket_name: String,
    #[allow(dead_code)] // TODO is read
    pub kafka_producer: FutureProducer,
    #[allow(dead_code)] // TODO is read
    pub kafka_consumer: StreamConsumer,
    _minio_container: testcontainers::ContainerAsync<MinIO>,
    _kafka_container: testcontainers::ContainerAsync<Kafka>,
}

impl TestHarness {
    pub(crate) async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
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

        let s3_client = aws_sdk_s3::Client::new(&config);
        let bucket_name =
            format!("test-bucket-{}", Uuid::new_v5(&Uuid::NAMESPACE_OID, s3_endpoint.as_bytes()));

        s3_client.create_bucket().bucket(&bucket_name).send().await?;

        let kafka_container = Kafka::default().start().await?;
        let bootstrap_servers =
            format!("127.0.0.1:{}", kafka_container.get_host_port_ipv4(kafka::KAFKA_PORT).await?);

        let kafka_producer = ClientConfig::new()
            .set("bootstrap.servers", &bootstrap_servers)
            .set("message.timeout.ms", "5000")
            .create::<FutureProducer>()
            .expect("Failed to create Kafka FutureProducer");

        let kafka_consumer = ClientConfig::new()
            .set("group.id", "testcontainer-rs")
            .set("bootstrap.servers", &bootstrap_servers)
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", "earliest")
            .create::<StreamConsumer>()
            .expect("Failed to create Kafka StreamConsumer");

        Ok(Self {
            s3_client,
            bucket_name,
            kafka_producer,
            kafka_consumer,
            _minio_container: minio_container,
            _kafka_container: kafka_container,
        })
    }
}
