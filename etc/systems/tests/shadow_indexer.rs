//! End-to-end test ensuring shadow-indexer boots, migrates, and persists nothing for committed
//! blocks.
//!
//! `shadow_blocks` holds reorged-out and reverted blocks only, so a devnet run that never reorgs
//! must leave the table empty. The table's existence is the liveness control proving the `ExEx`
//! writer actually started rather than the assertion passing vacuously.

use std::time::Duration;

use base_node_runner::FromExtensionConfig;
use base_shadow_indexer::{ShadowIndexerConfig, ShadowIndexerExtension};
use base_shadow_indexer_db::{PgConnectionParams, ShadowBlockRepo, ShadowDbConfig};
use base_system_tests::{SystemTestProviderExt, SystemTestStackBuilder};
use eyre::{Result, WrapErr, ensure};
use sqlx::postgres::PgPoolOptions;
use testcontainers::{ImageExt, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio::time::{Instant, sleep};

/// `testcontainers-modules` still defaults to Postgres 11, which predates the
/// `jsonb_path_query_array` used by migration 0004.
const POSTGRES_TAG: &str = "16-alpine";

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(60);
const DB_POLL_TIMEOUT: Duration = Duration::from_secs(20);
const DB_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[tokio::test]
async fn shadow_indexer_persists_no_canonical_blocks() -> Result<()> {
    let container = Postgres::default().with_tag(POSTGRES_TAG).start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let connection = PgConnectionParams {
        host: "127.0.0.1".to_string(),
        port,
        database: "postgres".to_string(),
        username: "postgres".to_string(),
        password: "postgres".to_string(),
    };

    let db_config = ShadowDbConfig {
        connection: connection.clone(),
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

    // Connect without `ShadowDbConfig::init_pool`: it would apply the migrations itself and make
    // the liveness assertions below pass even if the extension never started.
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(connection.connect_options())
        .await?;

    let shadow_blocks: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.shadow_blocks')::text")
            .fetch_one(&pool)
            .await?;
    ensure!(
        shadow_blocks.as_deref() == Some("shadow_blocks"),
        "shadow_blocks is missing, so the shadow indexer writer never ran its migrations: \
         to_regclass={shadow_blocks:?}, target_height={target}"
    );

    let repo = ShadowBlockRepo::new(pool);

    // Assert emptiness on every poll rather than once at the end: the writer flushes on a 1s tick,
    // so only sustained absence across the whole window rules out a late canonical flush.
    let deadline = Instant::now() + DB_POLL_TIMEOUT;
    let mut polls = 0usize;
    loop {
        let rows = repo
            .list_by_number_range(0, target as i64)
            .await
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        polls += 1;
        ensure!(
            rows.is_empty(),
            "shadow_blocks must stay empty for committed blocks: rows={}, numbers={:?}, \
             poll={polls}, target_height={target}",
            rows.len(),
            rows.iter().map(|row| row.number).collect::<Vec<_>>()
        );

        if Instant::now() >= deadline {
            break;
        }
        sleep(DB_POLL_INTERVAL).await;
    }

    Ok(())
}
