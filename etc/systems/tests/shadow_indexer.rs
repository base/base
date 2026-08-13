//! End-to-end test ensuring shadow-indexer persists committed blocks to Postgres.

use std::time::Duration;

use base_node_runner::FromExtensionConfig;
use base_shadow_indexer::{ShadowIndexerConfig, ShadowIndexerExtension};
use base_shadow_indexer_db::{ShadowBlockRepo, ShadowDbConfig};
use base_system_tests::{SystemTestProviderExt, SystemTestStackBuilder};
use eyre::{Result, WrapErr, ensure};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::time::{Instant, sleep};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(60);
const DB_POLL_TIMEOUT: Duration = Duration::from_secs(20);
const DB_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[tokio::test]
async fn shadow_indexer_persists_committed_blocks() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let db_config = ShadowDbConfig {
        url: url.clone(),
        max_connections: 5,
        connection_timeout: Duration::from_secs(5),
    };
    let ext = Box::new(ShadowIndexerExtension::from_config(ShadowIndexerConfig {
        enabled: true,
        db: db_config.clone(),
        builder_version: "e2e-test".to_string(),
    }));

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_builder_extension(ext)
        .build()
        .await?;

    let builder = system.l2_builder_provider()?;
    let target = builder
        .wait_for_block(5, BLOCK_PRODUCTION_TIMEOUT)
        .await
        .wrap_err("builder did not reach target block height")?;

    let pool = db_config.init_pool().await.map_err(|err| eyre::eyre!(err.to_string()))?;
    let repo = ShadowBlockRepo::new(pool);

    let deadline = Instant::now() + DB_POLL_TIMEOUT;
    let mut rows = repo
        .list_by_number_range(0, target as i64)
        .await
        .map_err(|err| eyre::eyre!(err.to_string()))?;
    while rows.len() < 3 && Instant::now() < deadline {
        sleep(DB_POLL_INTERVAL).await;
        rows = repo
            .list_by_number_range(0, target as i64)
            .await
            .map_err(|err| eyre::eyre!(err.to_string()))?;
    }

    let row_count = rows.len();
    let max_number = rows.iter().map(|row| row.number).max().unwrap_or(0);
    ensure!(
        row_count >= 3,
        "expected at least 3 shadow rows before timeout: rows={row_count}, target_height={target}"
    );
    ensure!(
        max_number >= 3,
        "expected shadow rows to reach height >= 3: max_number={max_number}, rows={row_count}, target_height={target}"
    );

    for row in &rows {
        ensure!(
            row.payload.builder_version == "e2e-test",
            "unexpected builder version: number={}, builder_version={}, target_height={target}",
            row.number,
            row.payload.builder_version
        );
        ensure!(
            !row.hash.is_empty(),
            "missing hash for shadow row: number={}, target_height={target}",
            row.number
        );
        ensure!(
            row.payload.block.is_object(),
            "missing serialized block for shadow row: number={}, target_height={target}",
            row.number
        );
    }

    Ok(())
}
