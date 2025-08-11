use flashblocks_archiver::database::Database;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

pub struct PostgresTestContainer {
    pub _container: testcontainers::ContainerAsync<Postgres>,
    pub database: Database,
    pub database_url: String,
}

impl PostgresTestContainer {
    pub async fn new(db_name: &str) -> anyhow::Result<Self> {
        use testcontainers::{runners::AsyncRunner, ImageExt};

        let postgres_image = Postgres::default()
            .with_db_name(db_name)
            .with_user("test_user")
            .with_password("test_password")
            .with_tag("16-alpine");

        let container = postgres_image.start().await?;
        let port = container.get_host_port_ipv4(5432).await?;

        let database_url = format!(
            "postgresql://test_user:test_password@localhost:{}/{}",
            port, db_name
        );

        let database = Database::new(&database_url, 10).await?;
        database.run_migrations().await?;

        Ok(Self {
            _container: container,
            database,
            database_url,
        })
    }

    pub async fn create_test_builder(&self, url: &str, name: Option<&str>) -> anyhow::Result<Uuid> {
        self.database.get_or_create_builder(url, name).await
    }
}
