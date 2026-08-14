//! Postgres-backed system tests for shadow block metrics polling.

use std::{collections::BTreeSet, time::Duration};

use alloy_consensus::{Block, BlockBody, Header, SignableTransaction, TxEip1559};
use alloy_primitives::{Address, Signature};
use anyhow::Result;
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use base_shadow_indexer_db::{
    ShadowBlockCursor, ShadowBlockPayload, ShadowBlockRepo, ShadowBlockRow, ShadowDbConfig,
    ShadowMetricsCursorRepo,
};
use base_shadow_metrics::{ShadowMetricsReader, ShadowMetricsReaderConfig, ShadowMetricsStore};
use chrono::Utc;
use reth_primitives_traits::RecoveredBlock;
use serde_json::json;
use sqlx::{PgPool, types::Json};
use testcontainers::{ContainerAsync, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio::time::sleep;

const DEFAULT_TEST_MAX_ROWS: u32 = 100;
const TEST_POLL_INTERVAL: Duration = Duration::from_secs(1);

struct TestDatabase {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDatabase {
    async fn start() -> Result<Self> {
        let container = Postgres::default().start().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        let pool =
            ShadowDbConfig { url, max_connections: 5, connection_timeout: Duration::from_secs(5) }
                .init_pool()
                .await?;

        Ok(Self { _container: container, pool })
    }

    async fn reader(&self, max_rows_per_poll: u32) -> Result<ShadowMetricsReader> {
        ShadowMetricsReader::new(
            ShadowMetricsStore::new(self.pool.clone()),
            ShadowMetricsReaderConfig { poll_interval: TEST_POLL_INTERVAL, max_rows_per_poll },
        )
        .await
    }
}

struct ShadowBlockFixture {
    number: u64,
    gas_used: u64,
    base_fee_per_gas: u64,
    deposit_count: usize,
    tips: Vec<u128>,
    builder_version: String,
}

impl ShadowBlockFixture {
    fn new(number: u64) -> Self {
        Self {
            number,
            gas_used: 21_000,
            base_fee_per_gas: 100,
            deposit_count: 0,
            tips: Vec::new(),
            builder_version: format!("builder-{number}"),
        }
    }

    const fn gas_used(mut self, gas_used: u64) -> Self {
        self.gas_used = gas_used;
        self
    }

    const fn base_fee_per_gas(mut self, base_fee_per_gas: u64) -> Self {
        self.base_fee_per_gas = base_fee_per_gas;
        self
    }

    const fn deposits(mut self, deposit_count: usize) -> Self {
        self.deposit_count = deposit_count;
        self
    }

    fn tips(mut self, tips: &[u128]) -> Self {
        self.tips = tips.to_vec();
        self
    }

    fn builder_version(mut self, builder_version: &str) -> Self {
        self.builder_version = builder_version.to_string();
        self
    }

    fn into_row(
        self,
        hash_seed: u8,
        reorged_out: bool,
        canonical_hash_seed: Option<u8>,
    ) -> ShadowBlockRow {
        let number = self.number;
        let payload = self.into_payload();
        let now = Utc::now();

        ShadowBlockRow {
            number: i64::try_from(number).expect("fixture block number fits in i64"),
            hash: vec![hash_seed; 32],
            reorged_out,
            canonical_hash: canonical_hash_seed.map(|seed| vec![seed; 32]),
            created_at: now,
            updated_at: now,
            payload,
        }
    }

    fn into_payload(self) -> ShadowBlockPayload {
        let mut transactions = Vec::with_capacity(self.deposit_count + self.tips.len());
        transactions.extend(
            (0..self.deposit_count).map(|_| {
                BaseTxEnvelope::from(TxDeposit { gas_limit: 21_000, ..Default::default() })
            }),
        );
        transactions.extend(self.tips.iter().enumerate().map(|(nonce, tip)| {
            BaseTxEnvelope::Eip1559(
                TxEip1559 {
                    chain_id: 1,
                    nonce: u64::try_from(nonce).expect("fixture nonce fits in u64"),
                    gas_limit: 21_000,
                    max_fee_per_gas: u128::from(self.base_fee_per_gas) + tip,
                    max_priority_fee_per_gas: *tip,
                    ..Default::default()
                }
                .into_signed(Signature::test_signature()),
            )
        }));
        let senders = vec![Address::ZERO; transactions.len()];
        let block = Block::<BaseTxEnvelope> {
            header: Header {
                number: self.number,
                gas_used: self.gas_used,
                base_fee_per_gas: Some(self.base_fee_per_gas),
                ..Default::default()
            },
            body: BlockBody { transactions, ..Default::default() },
        };

        ShadowBlockPayload {
            builder_version: self.builder_version,
            block: RecoveredBlock::new_unhashed(block, senders),
            receipts: Vec::new(),
        }
    }
}

#[tokio::test]
async fn emits_only_reconciled_shadow_row_once() -> Result<()> {
    let database = TestDatabase::start().await?;
    let mut reader = database.reader(DEFAULT_TEST_MAX_ROWS).await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    let rows = [
        ShadowBlockFixture::new(7).builder_version("shadow").into_row(0x11, true, Some(0x91)),
        ShadowBlockFixture::new(7).builder_version("canonical").into_row(0x12, false, None),
    ];
    repo.insert_batch(&rows).await?;

    let emitted = reader.poll_once().await?;
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].number, 7);
    assert_eq!(emitted[0].builder_version, "shadow");
    assert!(reader.poll_once().await?.is_empty(), "row must not be emitted twice");

    Ok(())
}

#[tokio::test]
async fn emits_unreconciled_row_after_reconciliation() -> Result<()> {
    let database = TestDatabase::start().await?;
    let mut reader = database.reader(DEFAULT_TEST_MAX_ROWS).await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    repo.insert_batch(&[ShadowBlockFixture::new(11).into_row(0x21, false, None)]).await?;

    assert!(reader.poll_once().await?.is_empty());

    sleep(Duration::from_millis(2)).await;
    repo.insert_batch(&[ShadowBlockFixture::new(11).into_row(0x21, true, Some(0xa1))]).await?;

    let emitted = reader.poll_once().await?;
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].number, 11);
    assert!(reader.poll_once().await?.is_empty());

    Ok(())
}

#[tokio::test]
async fn skips_unwind_and_advances_cursor() -> Result<()> {
    let database = TestDatabase::start().await?;
    let mut reader = database.reader(DEFAULT_TEST_MAX_ROWS).await?;
    ShadowBlockRepo::new(database.pool.clone())
        .insert_batch(&[ShadowBlockFixture::new(21).into_row(0x31, true, None)])
        .await?;

    assert!(reader.poll_once().await?.is_empty());
    let cursor = ShadowMetricsCursorRepo::new(database.pool.clone())
        .load()
        .await?
        .expect("reader initialization persists a cursor");
    assert_eq!(cursor.number, 21);
    assert_eq!(cursor.hash, vec![0x31; 32]);

    Ok(())
}

/// Pins an accepted risk: one poison payload fails the whole poll and leaves the reader
/// stuck without emitting valid neighbors or advancing until the row is repaired.
#[tokio::test]
async fn poison_payload_fails_the_whole_poll() -> Result<()> {
    let database = TestDatabase::start().await?;
    let mut reader = database.reader(DEFAULT_TEST_MAX_ROWS).await?;
    let repo = ShadowBlockRepo::new(database.pool.clone());
    let cursor_repo = ShadowMetricsCursorRepo::new(database.pool.clone());
    let initial_cursor =
        cursor_repo.load().await?.expect("reader initialization persists a cursor");
    repo.insert_batch(&[ShadowBlockFixture::new(31).into_row(0x41, true, Some(0xb1))]).await?;
    sleep(Duration::from_millis(2)).await;
    sqlx::query(
        "INSERT INTO shadow_blocks \
         (number, hash, reorged_out, canonical_hash, payload) \
         VALUES ($1, $2, true, $3, $4)",
    )
    .bind(32_i64)
    .bind(vec![0x42_u8; 32])
    .bind(vec![0xb2_u8; 32])
    .bind(Json(json!({ "builder_version": "poison" })))
    .execute(&database.pool)
    .await?;
    sleep(Duration::from_millis(2)).await;
    repo.insert_batch(&[ShadowBlockFixture::new(33).into_row(0x43, true, Some(0xb3))]).await?;

    let error = reader.poll_once().await.expect_err("poison payload must fail the whole poll");
    let error_chain = format!("{error:#}");
    assert!(
        error_chain.contains("payload")
            && error_chain.contains("missing field")
            && error_chain.contains("block"),
        "unexpected poll error: {error_chain}"
    );
    assert_eq!(cursor_repo.load().await?, Some(initial_cursor.clone()));
    assert!(reader.poll_once().await.is_err(), "reader must remain stuck before repair");
    assert_eq!(cursor_repo.load().await?, Some(initial_cursor));

    Ok(())
}

#[tokio::test]
async fn counts_deposits_but_excludes_them_from_fee_ordering() -> Result<()> {
    let database = TestDatabase::start().await?;
    let mut reader = database.reader(DEFAULT_TEST_MAX_ROWS).await?;
    ShadowBlockRepo::new(database.pool.clone())
        .insert_batch(&[ShadowBlockFixture::new(41)
            .gas_used(555_555)
            .base_fee_per_gas(0)
            .deposits(2)
            .tips(&[30, 20, 40])
            .into_row(0x51, true, Some(0xc1))])
        .await?;

    let emitted = reader.poll_once().await?;
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].gas_used, 555_555);
    assert_eq!(emitted[0].transaction_count, 5);
    assert_eq!(emitted[0].non_deposit_tx_count, 3);
    assert_eq!(emitted[0].priority_fee_inversions, 1);

    Ok(())
}

#[tokio::test]
async fn counts_sawtooth_boundaries_and_accepts_non_increasing_fees() -> Result<()> {
    let database = TestDatabase::start().await?;
    let mut reader = database.reader(DEFAULT_TEST_MAX_ROWS).await?;
    ShadowBlockRepo::new(database.pool.clone())
        .insert_batch(&[
            ShadowBlockFixture::new(51).tips(&[100, 90, 80, 120, 110, 100, 130, 120]).into_row(
                0x61,
                true,
                Some(0xd1),
            ),
            ShadowBlockFixture::new(52).tips(&[120, 110, 100, 90]).into_row(0x62, true, Some(0xd2)),
            ShadowBlockFixture::new(53).tips(&[50, 50, 40]).into_row(0x63, true, Some(0xd3)),
        ])
        .await?;

    let emitted = reader.poll_once().await?;
    assert_eq!(emitted.len(), 3);
    let sawtooth = emitted.iter().find(|stats| stats.number == 51).expect("sawtooth block");
    let descending = emitted.iter().find(|stats| stats.number == 52).expect("descending block");
    let equal_adjacent =
        emitted.iter().find(|stats| stats.number == 53).expect("equal-adjacent block");
    assert_eq!(sawtooth.priority_fee_inversions, 2);
    assert_eq!(descending.priority_fee_inversions, 0);
    assert_eq!(equal_adjacent.priority_fee_inversions, 0);

    Ok(())
}

#[tokio::test]
async fn persisted_cursor_never_moves_backwards() -> Result<()> {
    let database = TestDatabase::start().await?;
    let repo = ShadowMetricsCursorRepo::new(database.pool.clone());
    let updated_at = Utc::now();
    let older = ShadowBlockCursor { updated_at, number: 1, hash: vec![0x71; 32] };
    let newer = ShadowBlockCursor { updated_at, number: 2, hash: vec![0x72; 32] };

    repo.store(&older).await?;
    repo.store(&newer).await?;
    repo.store(&older).await?;

    assert_eq!(repo.load().await?, Some(newer));

    Ok(())
}

#[tokio::test]
async fn respects_poll_cap_and_advances_by_cap() -> Result<()> {
    const MAX_ROWS: u32 = 3;

    let database = TestDatabase::start().await?;
    let mut reader = database.reader(MAX_ROWS).await?;
    let rows = (1_u8..=7)
        .map(|offset| {
            ShadowBlockFixture::new(60 + u64::from(offset)).into_row(
                offset,
                true,
                Some(0xe0 + offset),
            )
        })
        .collect::<Vec<_>>();
    ShadowBlockRepo::new(database.pool.clone()).insert_batch(&rows).await?;

    let first = reader.poll_once().await?;
    assert_eq!(first.iter().map(|stats| stats.number).collect::<Vec<_>>(), [61, 62, 63]);
    let cursor = ShadowMetricsCursorRepo::new(database.pool.clone())
        .load()
        .await?
        .expect("reader initialization persists a cursor");
    assert_eq!(cursor.number, 63);

    let second = reader.poll_once().await?;
    assert_eq!(second.iter().map(|stats| stats.number).collect::<Vec<_>>(), [64, 65, 66]);
    let third = reader.poll_once().await?;
    assert_eq!(third.iter().map(|stats| stats.number).collect::<Vec<_>>(), [67]);
    assert!(reader.poll_once().await?.is_empty());

    Ok(())
}

#[tokio::test]
async fn drains_timestamp_tie_group_without_loss_or_duplicates() -> Result<()> {
    const MAX_ROWS: u32 = 10;
    const ROW_COUNT: usize = 55;
    const FIRST_NUMBER: u64 = 1_000;

    let database = TestDatabase::start().await?;
    let mut reader = database.reader(MAX_ROWS).await?;
    let rows = (0..ROW_COUNT)
        .map(|index| {
            let offset = u64::try_from(index).expect("fixture index fits in u64");
            ShadowBlockFixture::new(FIRST_NUMBER + offset).into_row(
                u8::try_from(index + 1).expect("fixture hash seed fits in u8"),
                true,
                Some(0xf1),
            )
        })
        .collect::<Vec<_>>();
    ShadowBlockRepo::new(database.pool.clone()).insert_batch(&rows).await?;

    let distinct_timestamps: i64 =
        sqlx::query_scalar("SELECT COUNT(DISTINCT updated_at) FROM shadow_blocks")
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(distinct_timestamps, 1, "fixture must form one timestamp tie group");

    let mut emitted_numbers = Vec::new();
    for _ in 0..10 {
        let batch = reader.poll_once().await?;
        assert!(batch.len() <= usize::try_from(MAX_ROWS).expect("poll cap fits in usize"));
        if batch.is_empty() {
            break;
        }
        emitted_numbers.extend(batch.into_iter().map(|stats| stats.number));
    }

    assert_eq!(emitted_numbers.len(), ROW_COUNT);
    let unique_numbers = emitted_numbers.iter().copied().collect::<BTreeSet<_>>();
    let expected_numbers = (FIRST_NUMBER
        ..FIRST_NUMBER + u64::try_from(ROW_COUNT).expect("row count fits in u64"))
        .collect::<BTreeSet<_>>();
    assert_eq!(unique_numbers.len(), ROW_COUNT, "tie group must not produce duplicates");
    assert_eq!(unique_numbers, expected_numbers, "tie group must not lose rows");
    assert!(reader.poll_once().await?.is_empty());

    Ok(())
}

#[tokio::test]
async fn resumes_from_persisted_cursor_after_restart() -> Result<()> {
    const MAX_ROWS: u32 = 1;

    let database = TestDatabase::start().await?;
    let mut reader = database.reader(MAX_ROWS).await?;
    ShadowBlockRepo::new(database.pool.clone())
        .insert_batch(&[
            ShadowBlockFixture::new(201).into_row(0x71, true, Some(0x81)),
            ShadowBlockFixture::new(202).into_row(0x72, true, Some(0x82)),
        ])
        .await?;

    let first = reader.poll_once().await?;
    assert_eq!(first.iter().map(|stats| stats.number).collect::<Vec<_>>(), [201]);
    drop(reader);

    let mut restarted_reader = database.reader(MAX_ROWS).await?;
    let resumed = restarted_reader.poll_once().await?;
    assert_eq!(resumed.iter().map(|stats| stats.number).collect::<Vec<_>>(), [202]);
    assert!(restarted_reader.poll_once().await?.is_empty());

    Ok(())
}
