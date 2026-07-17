//! Shadow builder canary: system-test primitives for the two release-canary
//! failure classes.
//!
//! - Validity: a follow-mode client must re-execute every block from the
//!   sequencer's combined builder to identical block hashes.
//! - Liveness (primary "would halt the chain" signal): the combined builder must
//!   keep producing blocks at slot cadence without stalling.
//!
//! The ExEx-based block-capture canary is exercised in `shadow_s6_reorg_capture`,
//! which drives a shadow-drive client and asserts end-to-end reorg capture.

use std::{collections::BTreeMap, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use base_shadow_canary::ShadowCanaryConfig;
use base_shadow_canary_db::{ShadowBlockRepo, ShadowDbConfig, ShadowBlockRow};
use base_system_tests::SystemTestStackBuilder;
use eyre::{Result, WrapErr};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::time::{sleep, timeout};

const L1_CHAIN_ID: u64 = 1337;
const L2_CHAIN_ID: u64 = 84538453;
const BLOCK_PRODUCTION_TIMEOUT: Duration = Duration::from_secs(30);
const FOLLOWER_SYNC_TIMEOUT: Duration = Duration::from_secs(60);
const BLOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);

const TARGET_BUILDER_BLOCK: u64 = 5;
const PARITY_BLOCKS: u64 = 3;

const LIVENESS_OBSERVE_BLOCKS: u64 = 5;
const MAX_BLOCK_STALL: Duration = Duration::from_secs(15);
const CADENCE_BLOCKS: u64 = 4;
const MAX_TS_GAP_SECS: u64 = 8;

const SHADOW_DRIVE_BUILD_DEADLINE_MS: u64 = 2000;
const SHADOW_DRIVE_MAX_REORG_DEPTH: u64 = 1;

static SHADOW_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn shadow_builder_follow_mode_block_parity() -> Result<()> {
    let _guard = SHADOW_TEST_LOCK.lock().await;
    base_node_runner::test_utils::init_silenced_tracing();

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_follow_mode_client_consensus()
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    let follower_provider = system.l2_client_provider()?;

    wait_for_block(&builder_provider, TARGET_BUILDER_BLOCK)
        .await
        .wrap_err("builder block production timed out")?;

    wait_for_block_with_timeout(&follower_provider, PARITY_BLOCKS, FOLLOWER_SYNC_TIMEOUT)
        .await
        .wrap_err("follow-mode client failed to sync")?;

    for number in 1..=PARITY_BLOCKS {
        let builder_hash = block_hash(&builder_provider, number)
            .await
            .wrap_err_with(|| format!("missing builder block {number}"))?;
        let follower_hash = block_hash(&follower_provider, number)
            .await
            .wrap_err_with(|| format!("missing follower block {number}"))?;

        assert_eq!(
            builder_hash, follower_hash,
            "block {number} hash diverged between builder and follow-mode client"
        );
    }

    Ok(())
}

#[tokio::test]
async fn shadow_builder_block_production_liveness() -> Result<()> {
    let _guard = SHADOW_TEST_LOCK.lock().await;
    base_node_runner::test_utils::init_silenced_tracing();

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;

    let mut last_block = wait_for_block(&builder_provider, 1).await?;
    let mut last_advance = tokio::time::Instant::now();
    let mut observed = 0u64;

    while observed < LIVENESS_OBSERVE_BLOCKS {
        let stalled_for = last_advance.elapsed();
        assert!(
            stalled_for < MAX_BLOCK_STALL,
            "builder stalled for {stalled_for:?} at block {last_block}"
        );

        let current = builder_provider.get_block_number().await?;
        if current > last_block {
            observed += current - last_block;
            last_block = current;
            last_advance = tokio::time::Instant::now();
        } else {
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    }

    Ok(())
}

#[tokio::test]
async fn shadow_builder_block_timestamp_cadence() -> Result<()> {
    let _guard = SHADOW_TEST_LOCK.lock().await;
    base_node_runner::test_utils::init_silenced_tracing();

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    wait_for_block(&builder_provider, CADENCE_BLOCKS).await?;

    let mut prev_ts: Option<u64> = None;
    for number in 1..=CADENCE_BLOCKS {
        let ts = block_timestamp(&builder_provider, number)
            .await
            .wrap_err_with(|| format!("missing builder block {number}"))?;

        if let Some(prev) = prev_ts {
            assert!(ts > prev, "block {number} timestamp {ts} did not advance past {prev}");
            let delta = ts - prev;
            assert!(
                delta <= MAX_TS_GAP_SECS,
                "block {number} cadence gap {delta}s exceeds {MAX_TS_GAP_SECS}s"
            );
        }
        prev_ts = Some(ts);
    }

    Ok(())
}

#[tokio::test]
async fn shadow_s6_reorg_capture() -> Result<()> {
    let _guard = SHADOW_TEST_LOCK.lock().await;
    base_node_runner::test_utils::init_silenced_tracing();

    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let db_config = ShadowDbConfig {
        url: database_url,
        max_connections: 5,
        connection_timeout: Duration::from_secs(5),
    };
    let pool = db_config
        .init_pool()
        .await
        .map_err(|error| eyre::eyre!(error))?;
    let repo = ShadowBlockRepo::new(pool);

    let system = SystemTestStackBuilder::new()
        .with_l1_chain_id(L1_CHAIN_ID)
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_shadow_canary_config(ShadowCanaryConfig {
            enabled: true,
            db: db_config.clone(),
            builder_version: "shadow-drive-system-test".to_string(),
        })
        .with_shadow_drive_client_consensus(
            SHADOW_DRIVE_BUILD_DEADLINE_MS,
            SHADOW_DRIVE_MAX_REORG_DEPTH,
        )
        .build()
        .await?;

    let builder_provider = system.l2_builder_provider()?;
    let shadow_provider = system.l2_shadow_drive_provider()?;

    let builder_tip = wait_for_block(&builder_provider, TARGET_BUILDER_BLOCK)
        .await
        .wrap_err("builder block production timed out")?;

    let observe_start = builder_tip.saturating_sub(PARITY_BLOCKS.saturating_sub(1)).max(1);
    let observe_end = builder_tip;

    for number in observe_start..=observe_end {
        wait_for_block_with_timeout(&shadow_provider, number, FOLLOWER_SYNC_TIMEOUT)
            .await
            .wrap_err_with(|| format!("shadow-drive client did not reach block {number}"))?;
        wait_for_shadow_match(&builder_provider, &shadow_provider, number)
            .await
            .wrap_err_with(|| format!("shadow-drive head drifted at block {number}"))?;
    }

    let shadow_rows = wait_for_shadow_rows(
        &repo,
        i64::try_from(observe_start)?,
        i64::try_from(observe_end)?,
    )
    .await?;

    let mut reorged_numbers = Vec::new();
    for row in &shadow_rows {
        if row.reorged_out {
            reorged_numbers.push(row.number);
        }
    }
    reorged_numbers.sort();
    reorged_numbers.dedup();

    assert!(
        !reorged_numbers.is_empty(),
        "expected at least one reorged-out shadow row in observed range"
    );

    for number_i64 in reorged_numbers {
        let number = u64::try_from(number_i64)?;
        let builder_hash = block_hash(&builder_provider, number)
            .await
            .wrap_err_with(|| format!("missing builder block {number}"))?
            .to_string();

        let reorged_row = shadow_rows.iter().find(|row| {
            row.number == number_i64
                && row.reorged_out
                && row.canonical_hash.as_deref() == Some(builder_hash.as_str())
        });
        assert!(
            reorged_row.is_some(),
            "missing reorged-out shadow row with canonical hash at block {number}"
        );

        let candidate_row = shadow_rows
            .iter()
            .find(|row| row.number == number_i64 && !row.reorged_out);
        assert!(
            candidate_row.is_some(),
            "missing committed shadow candidate row at block {number}"
        );
    }

    Ok(())
}

async fn wait_for_block(provider: &RootProvider<Base>, min_block: u64) -> Result<u64> {
    wait_for_block_with_timeout(provider, min_block, BLOCK_PRODUCTION_TIMEOUT).await
}

async fn wait_for_block_with_timeout(
    provider: &RootProvider<Base>,
    min_block: u64,
    deadline: Duration,
) -> Result<u64> {
    timeout(deadline, async {
        loop {
            let block = provider.get_block_number().await?;
            if block >= min_block {
                return Ok::<_, eyre::Error>(block);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("block production timed out")?
}

async fn block_hash(provider: &RootProvider<Base>, number: u64) -> Result<alloy_primitives::B256> {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await
        .wrap_err("failed to fetch block")?
        .ok_or_else(|| eyre::eyre!("block {number} not found"))?;
    Ok(block.header.hash_slow())
}

async fn block_timestamp(provider: &RootProvider<Base>, number: u64) -> Result<u64> {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await
        .wrap_err("failed to fetch block")?
        .ok_or_else(|| eyre::eyre!("block {number} not found"))?;
    Ok(block.header.timestamp)
}

async fn wait_for_shadow_rows(
    repo: &ShadowBlockRepo,
    start: i64,
    end: i64,
) -> Result<Vec<ShadowBlockRow>> {
    timeout(FOLLOWER_SYNC_TIMEOUT, async {
        loop {
            let rows = repo
                .list_by_number_range(start, end)
                .await
                .map_err(|error| eyre::eyre!(error))?;
            if shadow_rows_ready(&rows, start, end) {
                return Ok::<_, eyre::Error>(rows);
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("shadow canary rows did not become ready")?
}

async fn wait_for_shadow_match(
    builder: &RootProvider<Base>,
    shadow: &RootProvider<Base>,
    number: u64,
) -> Result<()> {
    timeout(FOLLOWER_SYNC_TIMEOUT, async {
        loop {
            let builder_hash = block_hash(builder, number).await?;
            let shadow_hash = block_hash(shadow, number).await?;
            if builder_hash == shadow_hash {
                return Ok::<_, eyre::Error>(());
            }
            sleep(BLOCK_POLL_INTERVAL).await;
        }
    })
    .await
    .wrap_err("shadow-drive head did not re-anchor to builder")?
}

fn shadow_rows_ready(rows: &[ShadowBlockRow], start: i64, end: i64) -> bool {
    let mut readiness: BTreeMap<i64, (bool, bool)> =
        (start..=end).map(|number| (number, (false, false))).collect();

    for row in rows {
        if let Some(flags) = readiness.get_mut(&row.number) {
            if row.reorged_out {
                flags.0 = true;
            } else {
                flags.1 = true;
            }
        }
    }

    let any_reorged = readiness.values().any(|(reorged, _)| *reorged);
    let any_committed = readiness.values().any(|(_, committed)| *committed);

    any_reorged && any_committed
}
