//! Independent hourly partition correctness against source-owned private PG17 fixtures.

pub mod common;

use std::{sync::Arc, time::Duration};

use audit_archiver_lib::{
    HourlyTransactionEventPartitions, PgTransactionEventSink, TransactionEventSink,
    index_transaction_event_partitions,
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use base_observability_events::TransactionEvent;
use chrono::{Duration as ChronoDuration, Utc};
use common::OwnedPostgres;
use serde_json::json;
use sqlx::{Connection, PgConnection};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
    time::{sleep, timeout},
};
use tower::ServiceExt;

/// Each test owns a separate PG17 cluster, nonsuperuser schema owner and runtime role.
#[derive(Debug)]
pub struct HourlyDatabase {
    /// Nonsuperuser writer/schema owner URL.
    pub url: String,
    /// Only this private cluster's fixture administrator.
    pub admin: PgConnection,
    /// Process/data directory lifetime.
    pub fixture: OwnedPostgres,
}
impl HourlyDatabase {
    /// Refuses utilities outside this checkout and validates the exact server directory.
    pub async fn new() -> anyhow::Result<Self> {
        let fixture = OwnedPostgres::new().await?;
        let mut admin = PgConnection::connect(&fixture.url).await?;
        sqlx::raw_sql("CREATE ROLE hourly_owner LOGIN;CREATE ROLE audit_archiver NOLOGIN;ALTER DATABASE postgres OWNER TO hourly_owner;ALTER SCHEMA public OWNER TO hourly_owner").execute(&mut admin).await?;
        let url = format!("postgres://hourly_owner@127.0.0.1:{}/postgres", fixture.port);
        Ok(Self { url, admin, fixture })
    }
    /// Waits for observable database state, never a timed fast-build assumption.
    pub async fn wait(&mut self, sql: &str) -> anyhow::Result<()> {
        timeout(Duration::from_secs(20), async {
            loop {
                if sqlx::query_scalar::<_, bool>(sql).fetch_one(&mut self.admin).await? {
                    return anyhow::Ok(());
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        Ok(())
    }
    /// Synthetic event-time fixture; arrival time remains real UTC.
    pub fn hourly_event(id: &str, time: chrono::DateTime<Utc>, kind: &str) -> TransactionEvent {
        serde_json::from_value(json!({"schema_version":"transaction-event/v1","event_id":id,"event_time":time,"producer":"base-builder","event_type":kind,"network":"base-mainnet","tx_hash":"0x1111111111111111111111111111111111111111111111111111111111111111","block_hash":null,"block_number":123,"payload_id":"hourly-bundle","request_id":null,"data":{"first":true,"bundle_id":"hourly-bundle","bundle_hash":"hourly-hash"}})).unwrap()
    }
    /// Explicit tiny-fixture owner preparation, never performed by normal migrate up.
    pub async fn prepare_hourly(conn: &mut PgConnection) -> anyhow::Result<()> {
        sqlx::query("DO $$DECLARE c text; d date; BEGIN FOR c IN SELECT unnest(ARRAY['hot','warm','cold']) LOOP PERFORM transaction_events_prepare_hourly(c); END LOOP; FOR c,d IN SELECT split_part(relname,'_',3),to_date(right(relname,8),'YYYYMMDD') FROM pg_class WHERE relnamespace='public'::regnamespace AND relname ~ '^transaction_events_(hot|warm)_[0-9]{8}$' LOOP PERFORM transaction_events_prepare_hourly(c,d); END LOOP; END $$").execute(conn).await?;
        Ok(())
    }
    /// Active forward-only fixture: missing historical BRIN coverage is permitted.
    pub async fn catalog_hourly(
        &mut self,
    ) -> anyhow::Result<(PgConnection, chrono::DateTime<Utc>)> {
        PgTransactionEventSink::migrate(&self.url).await?;
        let mut owner = PgConnection::connect(&self.url).await?;
        Self::prepare_hourly(&mut owner).await?;
        let day = Utc::now().date_naive() + ChronoDuration::days(7);
        HourlyTransactionEventPartitions::activate(&self.url, day, true).await?;
        Ok((owner, day.and_hms_opt(0, 0, 0).unwrap().and_utc()))
    }
}

/// Forward-only activation must preserve every old heap/index without backfilling.
#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
pub async fn forward_only_activation_preserves_mixed_index_coverage() -> anyhow::Result<()> {
    let db = HourlyDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut owner = PgConnection::connect(&db.url).await?;
    let sink = PgTransactionEventSink::connect(&db.url, 2).await?;
    sink.insert_events(&[
        HourlyDatabase::hourly_event("historical-hot", Utc::now(), "BUILDER_ACCEPTED"),
        HourlyDatabase::hourly_event("historical-cold", Utc::now(), "SIMULATION_FAILED"),
    ])
    .await?;
    let today = Utc::now().date_naive();
    let hot = format!("transaction_events_hot_{}", today.format("%Y%m%d"));
    sqlx::raw_sql(&format!("CREATE INDEX {hot}_ingested_at_idx ON {hot} USING brin(ingested_at); ALTER INDEX transaction_events_hot_ingested_at_idx ATTACH PARTITION {hot}_ingested_at_idx")).execute(&mut owner).await?;
    // Leave a real failed concurrent build on a different historical leaf.
    let warm = format!("transaction_events_warm_{}", today.format("%Y%m%d"));
    let mut blocker = PgConnection::connect(&db.url).await?;
    let mut held = blocker.begin().await?;
    sqlx::query(&format!("INSERT INTO {warm}(event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES('held-history','transaction-event/v1',now(),CURRENT_DATE,'warm','base-builder','SIMULATION_SUCCEEDED','{{}}')")).execute(&mut *held).await?;
    sqlx::query("SET lock_timeout='25ms'").execute(&mut owner).await?;
    let failed = sqlx::query(&format!(
        "CREATE INDEX CONCURRENTLY {warm}_ingested_at_idx ON {warm} USING brin(ingested_at)"
    ))
    .execute(&mut owner)
    .await
    .unwrap_err();
    assert_eq!(failed.as_database_error().and_then(|e| e.code()).as_deref(), Some("55P03"));
    sqlx::query("SET lock_timeout=0").execute(&mut owner).await?;
    held.rollback().await?;
    let invalid_oid: i64 = sqlx::query_scalar("SELECT indexrelid::bigint FROM pg_index WHERE indexrelid=to_regclass($1) AND NOT indisvalid").bind(format!("{warm}_ingested_at_idx")).fetch_one(&mut owner).await?;
    let day = today + ChronoDuration::days(7);
    HourlyDatabase::prepare_hourly(&mut owner).await?;
    // All table identities and physical index files, plus the COLD BRIN parent.
    sqlx::query("CREATE TEMP TABLE preserved AS SELECT c.oid,c.relfilenode,pg_relation_size(c.oid) AS bytes FROM pg_class c WHERE c.relnamespace='public'::regnamespace AND (c.relkind IN ('r','p','i') OR c.oid='transaction_events_cold_ingested_at_idx'::regclass)").execute(&mut owner).await?;
    let before_rows: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_agg(to_jsonb(t) ORDER BY event_id) FROM transaction_events t",
    )
    .fetch_one(&mut owner)
    .await?;
    let before_physical: serde_json::Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(i) ORDER BY i.indexrelid) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relkind='i'").fetch_one(&mut owner).await?;
    let old_parents: Vec<i64> = sqlx::query_scalar("SELECT indexrelid::bigint FROM pg_index WHERE indexrelid IN ('transaction_events_ingested_at_idx'::regclass,'transaction_events_hot_ingested_at_idx'::regclass,'transaction_events_warm_ingested_at_idx'::regclass) ORDER BY indexrelid").fetch_all(&mut owner).await?;
    let missing_before: Vec<String> = sqlx::query_scalar("SELECT c.relname::text FROM pg_partition_tree('transaction_events') t JOIN pg_class c ON c.oid=t.relid WHERE t.isleaf AND to_regclass(c.relname||'_ingested_at_idx') IS NULL ORDER BY c.relname").fetch_all(&mut owner).await?;
    HourlyTransactionEventPartitions::activate(&db.url, day, true).await?;
    let changed: i64 = sqlx::query_scalar("SELECT count(*) FROM preserved p LEFT JOIN pg_class c USING(oid) WHERE c.oid IS NULL OR c.relfilenode<>p.relfilenode OR pg_relation_size(c.oid)<>p.bytes").fetch_one(&mut owner).await?;
    assert_eq!(changed, 0);
    let after_rows: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_agg(to_jsonb(t) ORDER BY event_id) FROM transaction_events t",
    )
    .fetch_one(&mut owner)
    .await?;
    let after_physical: serde_json::Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(i) ORDER BY i.indexrelid) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relkind='i'").fetch_one(&mut owner).await?;
    assert_eq!(before_rows, after_rows);
    assert_eq!(before_physical, after_physical);
    let missing_after: Vec<String> = sqlx::query_scalar("SELECT c.relname::text FROM pg_partition_tree('transaction_events') t JOIN pg_class c ON c.oid=t.relid WHERE t.isleaf AND to_regclass(c.relname||'_ingested_at_idx') IS NULL ORDER BY c.relname").fetch_all(&mut owner).await?;
    assert_eq!(missing_before, missing_after);
    let remaining_old: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_class WHERE oid::bigint=ANY($1)")
            .bind(&old_parents)
            .fetch_one(&mut owner)
            .await?;
    assert_eq!(remaining_old, 0, "only approved partitioned metadata identities change");
    let invalid_after: i64 = sqlx::query_scalar("SELECT indexrelid::bigint FROM pg_index WHERE indexrelid=to_regclass($1) AND NOT indisvalid").bind(format!("{warm}_ingested_at_idx")).fetch_one(&mut owner).await?;
    assert_eq!(invalid_oid, invalid_after, "activation must not normalize interrupted old indexes");
    HourlyTransactionEventPartitions::validate(&mut owner).await?;
    HourlyTransactionEventPartitions::validate_indexes(&mut owner, false).await?;
    assert!(HourlyTransactionEventPartitions::validate_indexes(&mut owner, true).await.is_err());
    let hour = day.and_hms_opt(0, 0, 0).unwrap().and_utc();
    sqlx::query("SELECT transaction_events_create_hour('hot',$1)")
        .bind(hour)
        .execute(&mut owner)
        .await?;
    let hour_name = format!("transaction_events_hot_{}", hour.format("%Y%m%d%H"));
    let valid: bool = sqlx::query_scalar("SELECT i.indisvalid AND i.indisready AND EXISTS(SELECT 1 FROM pg_inherits e WHERE e.inhrelid=i.indexrelid AND e.inhparent=to_regclass($2)) FROM pg_index i WHERE i.indexrelid=to_regclass($1)").bind(format!("{hour_name}_ingested_at_idx")).bind(format!("transaction_events_hot_{}_ingested_at_idx",day.format("%Y%m%d"))).fetch_one(&mut owner).await?;
    assert!(valid);
    // Forced planner eligibility only, not a production cost/latency claim.
    // No event-date cutoff: freshly ingested late rows in old buckets stay valid.
    sqlx::query("SET enable_seqscan=off").execute(&mut owner).await?;
    let plan: serde_json::Value = sqlx::query_scalar(
        "EXPLAIN(FORMAT JSON) SELECT event_id FROM transaction_events WHERE ingested_at>=$1",
    )
    .bind(Utc::now() - ChronoDuration::days(1))
    .fetch_one(&mut owner)
    .await?;
    assert!(plan.to_string().contains(&format!("{hour_name}_ingested_at_idx")));
    sqlx::query("RESET enable_seqscan").execute(&mut owner).await?;
    // Normal migrate up remains schema-only and preserves the forward policy.
    PgTransactionEventSink::migrate(&db.url).await?;
    assert!(HourlyTransactionEventPartitions::validate_indexes(&mut owner, true).await.is_err());
    Ok(())
}

/// Unexpected metadata/PK/FK inventory must fail before any destructive step.
#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
pub async fn activation_unexpected_catalog_rolls_back() -> anyhow::Result<()> {
    for mutation in [
        "foreign_name",
        "options",
        "extra_index",
        "wrong_pk",
        "foreign_key",
        "root_oid",
        "extra_class",
        "wrong_day_name",
        "foreign_edge",
        "extra_leaf_unique",
    ] {
        let db = HourlyDatabase::new().await?;
        PgTransactionEventSink::migrate(&db.url).await?;
        let mut owner = PgConnection::connect(&db.url).await?;
        HourlyDatabase::prepare_hourly(&mut owner).await?;
        match mutation {
            "foreign_name" => {
                sqlx::raw_sql("ALTER INDEX transaction_events_ingested_at_idx RENAME TO preserved_parent;CREATE TABLE foreign_heap(ingested_at timestamptz);CREATE INDEX transaction_events_ingested_at_idx ON foreign_heap USING brin(ingested_at)").execute(&mut owner).await?;
            }
            "options" => {
                let leaf:String=sqlx::query_scalar("SELECT c.relname::text FROM pg_inherits t JOIN pg_class c ON c.oid=t.inhrelid WHERE t.inhparent='transaction_events_hot'::regclass ORDER BY c.relname LIMIT 1").fetch_one(&mut owner).await?;
                sqlx::query(&format!("CREATE INDEX {leaf}_ingested_at_idx ON {leaf} USING brin(ingested_at) WITH(pages_per_range=32)")).execute(&mut owner).await?;
            }
            "extra_index" => {
                sqlx::query(
                    "CREATE INDEX unrelated_parent_idx ON ONLY transaction_events_hot(event_id)",
                )
                .execute(&mut owner)
                .await?;
            }
            "wrong_pk" => {
                sqlx::raw_sql("ALTER TABLE transaction_events DROP CONSTRAINT transaction_events_pkey;ALTER TABLE transaction_events ADD PRIMARY KEY(retention_class,event_date,event_id)").execute(&mut owner).await?;
            }
            "foreign_key" => {
                let leaf:String=sqlx::query_scalar("SELECT c.relname::text FROM pg_partition_tree('transaction_events') t JOIN pg_class c ON c.oid=t.relid WHERE t.isleaf ORDER BY c.relname LIMIT 1").fetch_one(&mut owner).await?;
                sqlx::query(&format!("CREATE TABLE reference_heap(event_id text,retention_class text,event_date date,FOREIGN KEY(event_id,retention_class,event_date) REFERENCES {leaf}(event_id,retention_class,event_date))")).execute(&mut owner).await?;
            }
            "extra_class" => {
                sqlx::query("CREATE TABLE transaction_events_other PARTITION OF transaction_events FOR VALUES IN ('other')").execute(&mut owner).await?;
            }
            "wrong_day_name" => {
                let (leaf,day):(String,chrono::NaiveDate)=sqlx::query_as("SELECT c.relname::text,to_date(right(c.relname,8),'YYYYMMDD') FROM pg_inherits t JOIN pg_class c ON c.oid=t.inhrelid WHERE t.inhparent='transaction_events_cold'::regclass ORDER BY c.relname DESC LIMIT 1").fetch_one(&mut owner).await?;
                let alias = format!(
                    "transaction_events_cold_extra_{}",
                    (day - ChronoDuration::days(1)).format("%Y%m%d")
                );
                sqlx::query(&format!("ALTER TABLE {leaf} RENAME TO {alias}"))
                    .execute(&mut owner)
                    .await?;
            }
            "foreign_edge" => {
                let leaf:String=sqlx::query_scalar("SELECT c.relname::text FROM pg_inherits t JOIN pg_class c ON c.oid=t.inhrelid WHERE t.inhparent='transaction_events_hot'::regclass ORDER BY c.relname LIMIT 1").fetch_one(&mut owner).await?;
                sqlx::raw_sql(&format!("CREATE INDEX {leaf}_ingested_at_idx ON {leaf} USING brin(ingested_at);ALTER INDEX transaction_events_hot_ingested_at_idx ATTACH PARTITION {leaf}_ingested_at_idx;ALTER INDEX {leaf}_ingested_at_idx RENAME TO unrelated_preserved_brin")).execute(&mut owner).await?;
            }
            "extra_leaf_unique" => {
                let leaf:String=sqlx::query_scalar("SELECT c.relname::text FROM pg_inherits t JOIN pg_class c ON c.oid=t.inhrelid WHERE t.inhparent='transaction_events_hot'::regclass ORDER BY c.relname LIMIT 1").fetch_one(&mut owner).await?;
                sqlx::query(&format!(
                    "CREATE UNIQUE INDEX unrelated_unique_guard ON {leaf}(network)"
                ))
                .execute(&mut owner)
                .await?;
            }
            "root_oid" => {
                sqlx::query("UPDATE transaction_events_partition_policy SET root_oid=0")
                    .execute(&mut owner)
                    .await?;
            }
            _ => unreachable!(),
        }
        let before:serde_json::Value=sqlx::query_scalar("SELECT jsonb_build_object('classes',(SELECT jsonb_agg(to_jsonb(c) ORDER BY c.oid) FROM pg_class c WHERE c.relnamespace='public'::regnamespace),'indexes',(SELECT jsonb_agg(to_jsonb(i) ORDER BY i.indexrelid) FROM pg_index i),'edges',(SELECT jsonb_agg(to_jsonb(e) ORDER BY e.inhrelid,e.inhseqno) FROM pg_inherits e),'constraints',(SELECT jsonb_agg(to_jsonb(k) ORDER BY k.oid) FROM pg_constraint k WHERE k.connamespace='public'::regnamespace))").fetch_one(&mut owner).await?;
        let day = Utc::now().date_naive() + ChronoDuration::days(7);
        assert!(
            HourlyTransactionEventPartitions::activate(&db.url, day, true).await.is_err(),
            "mutation={mutation}"
        );
        let after:serde_json::Value=sqlx::query_scalar("SELECT jsonb_build_object('classes',(SELECT jsonb_agg(to_jsonb(c) ORDER BY c.oid) FROM pg_class c WHERE c.relnamespace='public'::regnamespace),'indexes',(SELECT jsonb_agg(to_jsonb(i) ORDER BY i.indexrelid) FROM pg_index i),'edges',(SELECT jsonb_agg(to_jsonb(e) ORDER BY e.inhrelid,e.inhseqno) FROM pg_inherits e),'constraints',(SELECT jsonb_agg(to_jsonb(k) ORDER BY k.oid) FROM pg_constraint k WHERE k.connamespace='public'::regnamespace))").fetch_one(&mut owner).await?;
        assert_eq!(before, after, "mutation={mutation} changed physical/catalog state");
        assert_eq!(HourlyTransactionEventPartitions::cutoff(&mut owner).await?, None);
    }
    Ok(())
}

/// Existing explicit index command can cancel and repair an interrupted hourly leaf.
#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
pub async fn manual_hourly_index_cancel_and_repair() -> anyhow::Result<()> {
    let mut db = HourlyDatabase::new().await?;
    let (mut owner, start) = db.catalog_hourly().await?;
    sqlx::query("SELECT transaction_events_create_hour('hot',$1)")
        .bind(start)
        .execute(&mut owner)
        .await?;
    let hour = format!("transaction_events_hot_{}", start.format("%Y%m%d%H"));
    // Fixture-only removal recreates the interrupted online-index starting state.
    sqlx::query("DROP INDEX transaction_events_ingested_at_idx").execute(&mut owner).await?;
    sqlx::raw_sql(include_str!("../migrations/002_transaction_events_ingested_at_index.sql"))
        .execute(&mut owner)
        .await?;
    let day_name = format!("transaction_events_hot_{}", start.format("%Y%m%d"));
    sqlx::raw_sql(&format!("CREATE INDEX {day_name}_ingested_at_idx ON ONLY {day_name} USING brin(ingested_at); ALTER INDEX transaction_events_hot_ingested_at_idx ATTACH PARTITION {day_name}_ingested_at_idx")).execute(&mut owner).await?;
    let earlier:Vec<(String,String)>=sqlx::query_as("SELECT c.relname::text,p.relname::text FROM pg_partition_tree('transaction_events') t JOIN pg_class c ON c.oid=t.relid JOIN pg_class p ON p.oid=t.parentrelid WHERE t.isleaf AND c.relname<$1 ORDER BY c.relname").bind(&hour).fetch_all(&mut owner).await?;
    for (leaf, parent) in earlier {
        sqlx::raw_sql(&format!("CREATE INDEX {leaf}_ingested_at_idx ON {leaf} USING brin(ingested_at);ALTER INDEX {parent}_ingested_at_idx ATTACH PARTITION {leaf}_ingested_at_idx")).execute(&mut owner).await?;
    }
    let mut unrelated = PgConnection::connect(&db.url).await?;
    let unrelated_pid: i32 =
        sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut unrelated).await?;
    let mut blocker = PgConnection::connect(&db.url).await?;
    let mut held = blocker.begin().await?;
    sqlx::query("INSERT INTO transaction_events(event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES('held-hour','transaction-event/v1',$1,$2,'hot','base-builder','BUILDER_ACCEPTED','{}')").bind(start).bind(start.date_naive()).execute(&mut *held).await?;
    let url = db.url.clone();
    let worker = tokio::spawn(async move { index_transaction_event_partitions(&url).await });
    db.wait(&format!("SELECT EXISTS(SELECT 1 FROM pg_stat_progress_create_index WHERE relid='{hour}'::regclass AND phase LIKE 'waiting%')")).await?;
    let (pid,backend_start):(i32,chrono::DateTime<Utc>)=sqlx::query_as("SELECT a.pid,a.backend_start FROM pg_stat_progress_create_index p JOIN pg_stat_activity a USING(pid) WHERE a.datname=current_database() AND a.usename='hourly_owner' AND p.relid=to_regclass($1)").bind(&hour).fetch_one(&mut db.admin).await?;
    assert_ne!(pid, unrelated_pid);
    let cancelled:bool=sqlx::query_scalar("SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE pid=$1 AND backend_start=$2 AND datname=current_database() AND usename='hourly_owner' AND query LIKE 'CREATE INDEX CONCURRENTLY%'").bind(pid).bind(backend_start).fetch_one(&mut db.admin).await?;
    assert!(cancelled);
    assert!(timeout(Duration::from_secs(10), worker).await??.is_err());
    let invalid: bool =
        sqlx::query_scalar("SELECT NOT indisvalid FROM pg_index WHERE indexrelid=to_regclass($1)")
            .bind(format!("{hour}_ingested_at_idx"))
            .fetch_one(&mut owner)
            .await?;
    assert!(invalid);
    held.rollback().await?;
    db.wait("SELECT NOT EXISTS(SELECT 1 FROM pg_stat_progress_create_index)").await?;
    let still_unrelated: i32 =
        sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut unrelated).await?;
    assert_eq!(still_unrelated, unrelated_pid);
    assert!(index_transaction_event_partitions(&db.url).await? > 0);
    HourlyTransactionEventPartitions::validate_indexes(&mut owner, true).await?;
    assert_eq!(index_transaction_event_partitions(&db.url).await?, 0);
    Ok(())
}

/// Preparation/detach signatures remain UTC/ISO even across caller GUC changes.
#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
pub async fn caller_datestyle_and_timezone_do_not_change_bucket_proof() -> anyhow::Result<()> {
    let mut db = HourlyDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut owner = PgConnection::connect(&db.url).await?;
    sqlx::raw_sql("SET datestyle='German, DMY';SET TimeZone='America/Los_Angeles'")
        .execute(&mut owner)
        .await?;
    HourlyDatabase::prepare_hourly(&mut owner).await?;
    let day = Utc::now().date_naive() + ChronoDuration::days(7);
    HourlyTransactionEventPartitions::activate(&db.url, day, true).await?;
    let hour = day.and_hms_opt(23, 0, 0).unwrap().and_utc();
    sqlx::query("SELECT transaction_events_create_hour('hot',$1)")
        .bind(hour)
        .execute(&mut owner)
        .await?;
    sqlx::query("SELECT transaction_events_detach_hour('hot',$1)")
        .bind(hour)
        .execute(&mut owner)
        .await?;
    let shape: serde_json::Value = sqlx::query_scalar(
        "SELECT member_shape FROM transaction_events_detached_partitions WHERE bucket_start=$1",
    )
    .bind(hour)
    .fetch_one(&mut owner)
    .await?;
    sqlx::raw_sql("SET datestyle='ISO, YMD';SET TimeZone='UTC'").execute(&mut owner).await?;
    let after: serde_json::Value = sqlx::query_scalar(
        "SELECT member_shape FROM transaction_events_detached_partitions WHERE bucket_start=$1",
    )
    .bind(hour)
    .fetch_one(&mut owner)
    .await?;
    assert_eq!(shape, after);
    sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
    let dropped: bool =
        sqlx::query_scalar("SELECT transaction_events_drop_detached_hour('hot',$1)")
            .bind(hour)
            .fetch_one(&mut db.admin)
            .await?;
    assert!(dropped);
    sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    Ok(())
}

/// An explicitly owned wire proxy that withholds a selected success response.
#[derive(Debug)]
pub struct ResponseProxy {
    /// Test-owned loopback listener URL.
    pub url: String,
    /// Confirms dispatch before response loss.
    pub triggered: Arc<Notify>,
    /// Proxy lifetime.
    pub task: tokio::task::JoinHandle<anyhow::Result<()>>,
}
impl Drop for ResponseProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl ResponseProxy {
    /// Models unknown commit acknowledgement, without changing the event timestamp.
    pub async fn start(url: &str, query: &'static [u8]) -> anyhow::Result<Self> {
        let options: sqlx::postgres::PgConnectOptions = url.parse()?;
        let backend = options.get_port();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let proxy_url = format!(
            "postgres://{}@127.0.0.1:{port}/{}",
            options.get_username(),
            options.get_database().unwrap()
        );
        let triggered = Arc::new(Notify::new());
        let observed = Arc::clone(&triggered);
        let task = tokio::spawn(async move {
            loop {
                let (mut client, _) = listener.accept().await?;
                let observed = Arc::clone(&observed);
                tokio::spawn(async move {
                    let mut server = TcpStream::connect(("127.0.0.1", backend)).await?;
                    let mut requests = [0u8; 4096];
                    let mut responses = [0u8; 4096];
                    let mut tail = Vec::new();
                    let mut dropping = false;
                    loop {
                        tokio::select! {
                            read = client.read(&mut requests) => {
                                let count = read?;
                                if count == 0 {
                                    return anyhow::Ok(());
                                }
                                tail.extend_from_slice(&requests[..count]);
                                if tail.windows(query.len()).any(|bytes| bytes == query) {
                                    dropping = true;
                                    observed.notify_one();
                                }
                                server.write_all(&requests[..count]).await?;
                                if tail.len() > 1024 {
                                    tail.drain(..tail.len() - 1024);
                                }
                            },
                            read = server.read(&mut responses) => {
                                let count = read?;
                                if count == 0 {
                                    return anyhow::Ok(());
                                }
                                if !dropping {
                                    client.write_all(&responses[..count]).await?;
                                }
                            }
                        }
                    }
                });
            }
        });
        Ok(Self { url: proxy_url, triggered, task })
    }
}
#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
/// Same-OID subtree mutations never authorize deleting altered retained members.
pub async fn hourly_detached_member_shape_changes_are_not_drop_authority() -> anyhow::Result<()> {
    for change in ["rename", "rebound", "reparent", "primary_key", "deferrable", "outside_data"] {
        let mut db = HourlyDatabase::new().await?;
        let (mut owner, hour) = db.catalog_hourly().await?;
        let day = hour.date_naive();
        let name = format!("transaction_events_hot_{}", hour.format("%Y%m%d%H"));
        let day_name = format!("transaction_events_hot_{}", hour.format("%Y%m%d"));
        sqlx::query("SELECT transaction_events_create_hour('hot',$1)")
            .bind(hour)
            .execute(&mut owner)
            .await?;
        sqlx::query("INSERT INTO transaction_events(event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES('member-sentinel','transaction-event/v1',$1,$2,'hot','base-builder','BUILDER_ACCEPTED','{\"sentinel\":true}')").bind(hour).bind(day).execute(&mut owner).await?;
        sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
        sqlx::query("SELECT transaction_events_detach_partition('hot',$1)")
            .bind(day)
            .execute(&mut db.admin)
            .await?;
        sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
        let proof: serde_json::Value = sqlx::query_scalar("SELECT to_jsonb(d) FROM transaction_events_detached_partitions d WHERE relation_name=$1").bind(&day_name).fetch_one(&mut owner).await?;
        match change {
            "rename" => {
                sqlx::query(&format!("ALTER TABLE {name} RENAME TO unrelated_preserved_heap"))
                    .execute(&mut owner)
                    .await?;
            }
            "rebound" => {
                sqlx::query(&format!(
                    "ALTER TABLE {name} DROP CONSTRAINT IF EXISTS audit_detached_bucket_guard"
                ))
                .execute(&mut owner)
                .await?;
                sqlx::query(&format!("ALTER TABLE {day_name} DETACH PARTITION {name}"))
                    .execute(&mut owner)
                    .await?;
                sqlx::query(&format!("DELETE FROM {name}")).execute(&mut owner).await?;
                sqlx::query(&format!("ALTER TABLE {day_name} ATTACH PARTITION {name} FOR VALUES FROM ($1) TO ($2)").replace("$1", &format!("'{}'", hour+ChronoDuration::days(100))).replace("$2", &format!("'{}'", hour+ChronoDuration::days(100)+ChronoDuration::hours(1)))).execute(&mut owner).await?;
                sqlx::query(&format!("INSERT INTO {name}(event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES('future-sentinel','transaction-event/v1',$1,$2,'hot','base-builder','BUILDER_ACCEPTED','{{\"future\":true}}')")).bind(hour+ChronoDuration::days(100)).bind(day+ChronoDuration::days(100)).execute(&mut owner).await?;
            }
            "reparent" => {
                let next_day = day + ChronoDuration::days(100);
                let next_parent = format!("transaction_events_hot_{}", next_day.format("%Y%m%d"));
                sqlx::query("SELECT transaction_events_create_partition('hot',$1)")
                    .bind(next_day)
                    .execute(&mut owner)
                    .await?;
                sqlx::query(&format!("ALTER TABLE {day_name} DETACH PARTITION {name}"))
                    .execute(&mut owner)
                    .await?;
                sqlx::query(&format!(
                    "ALTER TABLE {name} DROP CONSTRAINT audit_detached_bucket_guard"
                ))
                .execute(&mut owner)
                .await?;
                sqlx::query(&format!("DELETE FROM {name}")).execute(&mut owner).await?;
                sqlx::query(&format!("ALTER TABLE {next_parent} ATTACH PARTITION {name} FOR VALUES FROM ('{}') TO ('{}')",hour+ChronoDuration::days(100),hour+ChronoDuration::days(100)+ChronoDuration::hours(1))).execute(&mut owner).await?;
                sqlx::query(&format!("INSERT INTO {name}(event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES('future-sentinel','transaction-event/v1',$1,$2,'hot','base-builder','BUILDER_ACCEPTED','{{\"future\":true}}')")).bind(hour+ChronoDuration::days(100)).bind(next_day).execute(&mut owner).await?;
            }
            "primary_key" => {
                sqlx::query(&format!("ALTER TABLE {name} DROP CONSTRAINT {name}_pkey"))
                    .execute(&mut owner)
                    .await?;
            }
            "deferrable" => {
                sqlx::raw_sql(&format!("ALTER TABLE {name} DROP CONSTRAINT {name}_pkey; ALTER TABLE {name} ADD PRIMARY KEY(event_id) DEFERRABLE INITIALLY IMMEDIATE")).execute(&mut owner).await?;
            }
            "outside_data" => {
                // The detached day no longer supplies its inherited class/date bound.
                let insertion = sqlx::query(&format!("INSERT INTO {name}(event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES('outside-sentinel','transaction-event/v1',$1,$2,'cold','base-builder','BUILDER_ACCEPTED','{{\"future\":true}}')")).bind(hour).bind(day+ChronoDuration::days(100)).execute(&mut owner).await;
                if insertion.is_err() {
                    // A persistent bound guard prevents adding unproven future data.
                    assert_eq!(
                        sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {name}"))
                            .fetch_one(&mut owner)
                            .await?,
                        1
                    );
                    let after_proof: serde_json::Value = sqlx::query_scalar("SELECT to_jsonb(d) FROM transaction_events_detached_partitions d WHERE relation_name=$1").bind(&day_name).fetch_one(&mut owner).await?;
                    assert_eq!(proof, after_proof);
                    sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
                    sqlx::query("SELECT transaction_events_drop_detached_partition('hot',$1)")
                        .bind(day)
                        .execute(&mut db.admin)
                        .await?;
                    sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
                    continue;
                }
            }
            _ => unreachable!(),
        }
        sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
        let dropped = sqlx::query("SELECT transaction_events_drop_detached_partition('hot',$1)")
            .bind(day)
            .execute(&mut db.admin)
            .await;
        sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
        assert!(dropped.is_err(), "changed member {change} must fail closed before DROP");
        let actual_proof: serde_json::Value = sqlx::query_scalar("SELECT to_jsonb(d) FROM transaction_events_detached_partitions d WHERE relation_name=$1").bind(&day_name).fetch_one(&mut owner).await?;
        assert_eq!(proof, actual_proof);
        let member_name =
            if change == "rename" { "unrelated_preserved_heap" } else { name.as_str() };
        if change == "rebound" || change == "reparent" {
            let (time, data): (chrono::DateTime<Utc>, serde_json::Value) =
                sqlx::query_as(&format!("SELECT event_time,data FROM {member_name}"))
                    .fetch_one(&mut owner)
                    .await?;
            assert_eq!(time, hour + ChronoDuration::days(100));
            assert_eq!(data, json!({"future":true}));
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {member_name}"))
                .fetch_one(&mut owner)
                .await?,
            if change == "outside_data" { 2 } else { 1 }
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
/// Detached future data is either prevented by the original guard or preserved.
pub async fn hourly_detached_heap_future_data_is_guarded_or_preserved() -> anyhow::Result<()> {
    for tampered in [false, true] {
        let mut db = HourlyDatabase::new().await?;
        let (mut owner, hour) = db.catalog_hourly().await?;
        let name = format!("transaction_events_hot_{}", hour.format("%Y%m%d%H"));
        sqlx::query("SELECT transaction_events_create_hour('hot',$1)")
            .bind(hour)
            .execute(&mut owner)
            .await?;
        sqlx::query("SELECT transaction_events_detach_hour('hot',$1)")
            .bind(hour)
            .execute(&mut owner)
            .await?;
        let proof:serde_json::Value=sqlx::query_scalar("SELECT to_jsonb(d) FROM transaction_events_detached_partitions d WHERE relation_name=$1").bind(&name).fetch_one(&mut owner).await?;
        if tampered {
            sqlx::query(&format!("ALTER TABLE {name} DROP CONSTRAINT audit_detached_bucket_guard"))
                .execute(&mut owner)
                .await?;
        }
        let inserted=sqlx::query(&format!("INSERT INTO {name}(event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES('future-sentinel','transaction-event/v1',$1,$2,'hot','base-builder','BUILDER_ACCEPTED','{{\"bytes\":\"preserve-future\"}}')")).bind(hour+ChronoDuration::days(100)).bind(hour.date_naive()+ChronoDuration::days(100)).execute(&mut owner).await;
        if tampered {
            inserted?;
        } else {
            assert_eq!(
                inserted.unwrap_err().as_database_error().unwrap().code().as_deref(),
                Some("23514")
            );
        }
        sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
        let dropped = sqlx::query("SELECT transaction_events_drop_detached_hour('hot',$1)")
            .bind(hour)
            .execute(&mut db.admin)
            .await;
        sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
        if tampered {
            assert!(dropped.is_err());
            let after:serde_json::Value=sqlx::query_scalar("SELECT to_jsonb(d) FROM transaction_events_detached_partitions d WHERE relation_name=$1").bind(&name).fetch_one(&mut owner).await?;
            assert_eq!(proof, after);
            let (time, data): (chrono::DateTime<Utc>, serde_json::Value) =
                sqlx::query_as(&format!("SELECT event_time,data FROM {name}"))
                    .fetch_one(&mut owner)
                    .await?;
            assert_eq!(time, hour + ChronoDuration::days(100));
            assert_eq!(data, json!({"bytes":"preserve-future"}));
        } else {
            dropped?;
        }
    }
    Ok(())
}

/// Same-key deferrable PKs cannot be adopted as targetless ON CONFLICT arbiters.
#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
pub async fn hourly_deferrable_identity_is_refused_without_catalog_repair() -> anyhow::Result<()> {
    for bucket in ["hot", "warm", "hour"] {
        let mut db = HourlyDatabase::new().await?;
        let (mut owner, hour) = db.catalog_hourly().await?;
        let (class, time, name, keys) = if bucket == "hour" {
            sqlx::query("SELECT transaction_events_create_hour('hot',$1)")
                .bind(hour)
                .execute(&mut owner)
                .await?;
            ("hot", hour, format!("transaction_events_hot_{}", hour.format("%Y%m%d%H")), "event_id")
        } else {
            let time = Utc::now();
            (
                bucket,
                time,
                format!("transaction_events_{bucket}_{}", time.format("%Y%m%d")),
                "event_id,retention_class,event_date",
            )
        };
        sqlx::raw_sql(&format!("ALTER TABLE {name} DROP CONSTRAINT {name}_pkey; ALTER TABLE {name} ADD PRIMARY KEY ({keys}) DEFERRABLE INITIALLY IMMEDIATE")).execute(&mut owner).await?;
        let semantics: (bool, bool, bool) = sqlx::query_as("SELECT i.indimmediate,k.condeferrable,k.condeferred FROM pg_index i JOIN pg_constraint k ON k.conindid=i.indexrelid WHERE i.indrelid=to_regclass($1) AND i.indisprimary").bind(&name).fetch_one(&mut owner).await?;
        assert_eq!(semantics, (false, true, false));
        // Demonstrate the actual writer incompatibility, not just a catalog bit.
        sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
        let write = sqlx::query("INSERT INTO transaction_events(event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES('deferrable-probe','transaction-event/v1',$1,$2,$3,'base-builder','BUILDER_ACCEPTED','{}') ON CONFLICT DO NOTHING").bind(time).bind(time.date_naive()).bind(class).execute(&mut db.admin).await.unwrap_err();
        assert_eq!(write.as_database_error().and_then(|e| e.code()).as_deref(), Some("55000"));
        eprintln!("{bucket}: reproduced targetless root writer refusal: {write}");
        let before: serde_json::Value =
            sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(i) ORDER BY indexrelid) FROM pg_index i")
                .fetch_one(&mut owner)
                .await?;
        let (functions, value) = if bucket == "hour" {
            (["validate_hour", "create_hour", "detach_hour"], time.to_rfc3339())
        } else {
            (
                ["validate_day", "create_partition", "detach_partition"],
                time.date_naive().to_string(),
            )
        };
        for function in functions {
            let cast = if bucket == "hour" { "timestamptz" } else { "date" };
            assert!(
                sqlx::query(&format!("SELECT transaction_events_{function}($1,$2::{cast})"))
                    .bind(class)
                    .bind(&value)
                    .execute(&mut db.admin)
                    .await
                    .is_err(),
                "{bucket} {function} must refuse a deferrable PK"
            );
        }
        sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
        assert!(HourlyTransactionEventPartitions::validate(&mut owner).await.is_err());
        assert!(HourlyTransactionEventPartitions::horizons(&mut owner, time).await.is_err());
        let sink = PgTransactionEventSink::connect(&db.url, 2).await?;
        assert!(sink.maintain_partitions_at(time).await.is_err());
        assert!(index_transaction_event_partitions(&db.url).await.is_err());
        let start = if bucket == "hour" {
            time
        } else {
            time.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc()
        };
        let end = start
            + if bucket == "hour" { ChronoDuration::hours(1) } else { ChronoDuration::days(1) };
        assert!(
            sqlx::query("SELECT transaction_events_record_detach(to_regclass($1)::oid,$2,$3,$4)")
                .bind(&name)
                .bind(class)
                .bind(start)
                .bind(end)
                .execute(&mut owner)
                .await
                .is_err(),
            "owner-only proof capture must refuse non-immediate arbiters too"
        );
        let after: serde_json::Value =
            sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(i) ORDER BY indexrelid) FROM pg_index i")
                .fetch_one(&mut owner)
                .await?;
        assert_eq!(before, after);
        let proof_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM transaction_events_detached_partitions")
                .fetch_one(&mut owner)
                .await?;
        assert_eq!(proof_count, 0);
        // Only the fixture owner repairs; normal paths above never rewrite it.
        sqlx::raw_sql(&format!("ALTER TABLE {name} DROP CONSTRAINT {name}_pkey; ALTER TABLE {name} ADD PRIMARY KEY ({keys})")).execute(&mut owner).await?;
        HourlyTransactionEventPartitions::validate(&mut owner).await?;
        sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
        for expected in [1, 0] {
            let rows = sqlx::query("INSERT INTO transaction_events(event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES('deferrable-probe','transaction-event/v1',$1,$2,$3,'base-builder','BUILDER_ACCEPTED','{}') ON CONFLICT DO NOTHING").bind(time).bind(time.date_naive()).bind(class).execute(&mut db.admin).await?.rows_affected();
            assert_eq!(
                rows, expected,
                "explicit immediate fixture repair restores first-winner dedup"
            );
        }
        sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    }
    Ok(())
}

/// Expanded but inactive topology must refuse deferrable PKs before DDL/preparation.
#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
pub async fn hourly_null_cutoff_deferrable_primary_key_refuses_activation() -> anyhow::Result<()> {
    let mut db = HourlyDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut owner = PgConnection::connect(&db.url).await?;
    HourlyDatabase::prepare_hourly(&mut owner).await?;
    // Owner-induced drift affects both the parent and its retained daily PKs.
    sqlx::raw_sql("ALTER TABLE transaction_events DROP CONSTRAINT transaction_events_pkey; ALTER TABLE transaction_events ADD PRIMARY KEY(event_id,retention_class,event_date) DEFERRABLE INITIALLY IMMEDIATE").execute(&mut owner).await?;
    let now = Utc::now();
    let day = now.date_naive();
    let before: serde_json::Value = sqlx::query_scalar("SELECT jsonb_build_object('indexes',(SELECT jsonb_agg(to_jsonb(i) ORDER BY indexrelid) FROM pg_index i),'constraints',(SELECT jsonb_agg(to_jsonb(k) ORDER BY oid) FROM pg_constraint k),'policy',(SELECT to_jsonb(p) FROM transaction_events_partition_policy p),'proof',(SELECT jsonb_agg(to_jsonb(d)) FROM transaction_events_detached_partitions d))").fetch_one(&mut owner).await?;
    assert_eq!(HourlyTransactionEventPartitions::cutoff(&mut owner).await?, None);
    sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
    for function in ["validate_day", "create_partition", "detach_partition"] {
        assert!(
            sqlx::query(&format!("SELECT transaction_events_{function}('hot',$1)"))
                .bind(day)
                .execute(&mut db.admin)
                .await
                .is_err(),
            "NULL policy {function} must refuse deferrable PK"
        );
    }
    // A newly requested daily bucket must not inherit a deferrable parent arbiter.
    assert!(
        sqlx::query("SELECT transaction_events_create_partition('hot',$1)")
            .bind(day + ChronoDuration::days(20))
            .execute(&mut db.admin)
            .await
            .is_err()
    );
    sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    assert!(HourlyTransactionEventPartitions::validate(&mut owner).await.is_err());
    assert!(HourlyTransactionEventPartitions::horizons(&mut owner, now).await.is_err());
    let sink = PgTransactionEventSink::connect(&db.url, 2).await?;
    assert!(sink.maintain_partitions_at(now).await.is_err());
    assert!(
        sqlx::query("SELECT transaction_events_prepare_hourly('hot')")
            .execute(&mut owner)
            .await
            .is_err(),
        "class preparation must refuse the deferrable parent too"
    );
    assert!(
        sqlx::query("SELECT transaction_events_prepare_hourly('hot',$1)")
            .bind(day)
            .execute(&mut owner)
            .await
            .is_err()
    );
    assert!(
        HourlyTransactionEventPartitions::activate(&db.url, day + ChronoDuration::days(7), true)
            .await
            .is_err()
    );
    let after: serde_json::Value = sqlx::query_scalar("SELECT jsonb_build_object('indexes',(SELECT jsonb_agg(to_jsonb(i) ORDER BY indexrelid) FROM pg_index i),'constraints',(SELECT jsonb_agg(to_jsonb(k) ORDER BY oid) FROM pg_constraint k),'policy',(SELECT to_jsonb(p) FROM transaction_events_partition_policy p),'proof',(SELECT jsonb_agg(to_jsonb(d)) FROM transaction_events_detached_partitions d))").fetch_one(&mut owner).await?;
    assert_eq!(before, after, "refusal must not repair catalog or record detach authority");
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
/// Missing/wrong daily keys fail reuse, maintenance and coverage without repair.
pub async fn hourly_old_daily_primary_key_must_be_valid_before_reuse() -> anyhow::Result<()> {
    let mut db = HourlyDatabase::new().await?;
    let (mut owner, _) = db.catalog_hourly().await?;
    for class in ["hot", "warm"] {
        let (name,day):(String,chrono::NaiveDate)=sqlx::query_as("SELECT c.relname::text,to_date(right(c.relname,8),'YYYYMMDD') FROM pg_inherits t JOIN pg_class c ON c.oid=t.inhrelid WHERE t.inhparent=to_regclass($1) AND c.relkind='r' ORDER BY c.relname LIMIT 1").bind(format!("transaction_events_{class}")).fetch_one(&mut owner).await?;
        let pk:String=sqlx::query_scalar("SELECT conname::text FROM pg_constraint WHERE conrelid=to_regclass($1) AND contype='p'").bind(&name).fetch_one(&mut owner).await?;
        sqlx::query(&format!("ALTER TABLE {name} DROP CONSTRAINT {pk}"))
            .execute(&mut owner)
            .await?;
        for wrong_key in [false, true] {
            if wrong_key {
                sqlx::query(&format!("ALTER TABLE {name} ADD PRIMARY KEY(event_id)"))
                    .execute(&mut owner)
                    .await?;
            }
            sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
            for function in ["validate_day", "create_partition", "detach_partition"] {
                assert!(
                    sqlx::query(&format!("SELECT transaction_events_{function}($1,$2)"))
                        .bind(class)
                        .bind(day)
                        .execute(&mut db.admin)
                        .await
                        .is_err(),
                    "{class} {function} must reject wrong_key={wrong_key}"
                );
            }
            sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
            assert!(
                HourlyTransactionEventPartitions::horizons(&mut owner, Utc::now()).await.is_err()
            );
            assert!(HourlyTransactionEventPartitions::validate(&mut owner).await.is_err());
            let sink = PgTransactionEventSink::connect(&db.url, 2).await?;
            assert!(sink.maintain_partitions_at(Utc::now()).await.is_err());
            if wrong_key {
                sqlx::query(&format!("ALTER TABLE {name} DROP CONSTRAINT {name}_pkey"))
                    .execute(&mut owner)
                    .await?;
            }
        }
        // An explicit fixture-owner repair, never automatic retention repair.
        sqlx::query(&format!(
            "ALTER TABLE {name} ADD PRIMARY KEY(event_id,retention_class,event_date)"
        ))
        .execute(&mut owner)
        .await?;
        sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
        let changed: bool = sqlx::query_scalar("SELECT transaction_events_create_partition($1,$2)")
            .bind(class)
            .bind(day)
            .fetch_one(&mut db.admin)
            .await?;
        assert!(!changed);
        sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    }
    let day:chrono::NaiveDate=sqlx::query_scalar("SELECT to_date(right(c.relname,8),'YYYYMMDD') FROM pg_inherits t JOIN pg_class c ON c.oid=t.inhrelid WHERE t.inhparent='transaction_events_cold'::regclass ORDER BY c.relname LIMIT 1").fetch_one(&mut owner).await?;
    sqlx::query("SELECT transaction_events_validate_day('cold',$1)")
        .bind(day)
        .execute(&mut owner)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
/// Canonical names and replacement OIDs never authorize unrelated DROP.
pub async fn hourly_catalog_unrelated_table_is_not_drop_authority() -> anyhow::Result<()> {
    let mut db = HourlyDatabase::new().await?;
    let (mut owner, hour) = db.catalog_hourly().await?;
    let name = format!("transaction_events_hot_{}", hour.format("%Y%m%d%H"));
    sqlx::raw_sql(&format!(
        "CREATE TABLE {name}(sentinel bytea); INSERT INTO {name} VALUES (decode('00ff1234','hex'))"
    ))
    .execute(&mut owner)
    .await?;
    sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
    let result = sqlx::query("SELECT transaction_events_drop_detached_hour('hot',$1)")
        .bind(hour)
        .execute(&mut db.admin)
        .await;
    sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    let retained: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(&name)
        .fetch_one(&mut owner)
        .await?;
    assert!(retained, "unrelated canonical-named table was deleted");
    assert!(result.is_err(), "name-only drop must fail closed");
    let bytes: Vec<u8> =
        sqlx::query_scalar(&format!("SELECT sentinel FROM {name}")).fetch_one(&mut owner).await?;
    assert_eq!(bytes, vec![0, 255, 18, 52]);
    // Maintenance must not turn discovery by name into destructive authority.
    let sink = PgTransactionEventSink::connect(&db.url, 2).await?;
    assert!(sink.maintain_partitions_at(hour).await.is_err());
    for action in ["create", "detach"] {
        sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
        assert!(
            sqlx::query(&format!("SELECT transaction_events_{action}_hour('hot',$1)"))
                .bind(hour)
                .execute(&mut db.admin)
                .await
                .is_err()
        );
        sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    }
    let bytes: Vec<u8> =
        sqlx::query_scalar(&format!("SELECT sentinel FROM {name}")).fetch_one(&mut owner).await?;
    assert_eq!(bytes, vec![0, 255, 18, 52]);
    // A genuine detach record cannot authorize a replacement OID at the same name.
    let next_hour = hour + ChronoDuration::hours(2);
    let next_name = format!("transaction_events_hot_{}", next_hour.format("%Y%m%d%H"));
    sqlx::query("SELECT transaction_events_create_hour('hot',$1)")
        .bind(next_hour)
        .execute(&mut owner)
        .await?;
    sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
    sqlx::query("SELECT transaction_events_detach_hour('hot',$1)")
        .bind(next_hour)
        .execute(&mut db.admin)
        .await?;
    assert!(
        sqlx::query("DELETE FROM transaction_events_detached_partitions")
            .execute(&mut db.admin)
            .await
            .is_err()
    );
    sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    sqlx::raw_sql(&format!("ALTER TABLE {next_name} RENAME TO preserved_detached; CREATE TABLE {next_name}(sentinel bytea); INSERT INTO {next_name} VALUES (decode('deadbeef','hex'))")).execute(&mut owner).await?;
    sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
    assert!(
        sqlx::query("SELECT transaction_events_drop_detached_hour('hot',$1)")
            .bind(next_hour)
            .execute(&mut db.admin)
            .await
            .is_err()
    );
    sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    let bytes: Vec<u8> = sqlx::query_scalar(&format!("SELECT sentinel FROM {next_name}"))
        .fetch_one(&mut owner)
        .await?;
    assert_eq!(bytes, vec![222, 173, 190, 239]);
    sqlx::raw_sql(&format!(
        "DROP TABLE {next_name}; ALTER TABLE preserved_detached RENAME TO {next_name}"
    ))
    .execute(&mut owner)
    .await?;
    sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
    let dropped: bool =
        sqlx::query_scalar("SELECT transaction_events_drop_detached_hour('hot',$1)")
            .bind(next_hour)
            .fetch_one(&mut db.admin)
            .await?;
    assert!(dropped);
    sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    // The older day interface now guards hourly branches too, including COLD.
    let far_day = (hour + ChronoDuration::days(100)).date_naive();
    let day_name = format!("transaction_events_cold_{}", far_day.format("%Y%m%d"));
    sqlx::raw_sql(&format!("CREATE TABLE {day_name}(sentinel bytea); INSERT INTO {day_name} VALUES (decode('aa','hex'))")).execute(&mut owner).await?;
    sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
    assert!(
        sqlx::query("SELECT transaction_events_drop_detached_partition('cold',$1)")
            .bind(far_day)
            .execute(&mut db.admin)
            .await
            .is_err()
    );
    sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    let bytes: Vec<u8> = sqlx::query_scalar(&format!("SELECT sentinel FROM {day_name}"))
        .fetch_one(&mut owner)
        .await?;
    assert_eq!(bytes, vec![170]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
/// Canonical hour names do not substitute for exact UTC bounds.
pub async fn hourly_catalog_wrong_bounds_fail_create_and_coverage() -> anyhow::Result<()> {
    let mut db = HourlyDatabase::new().await?;
    let (mut owner, start) = db.catalog_hourly().await?;
    sqlx::query("SET timezone='America/Los_Angeles'").execute(&mut owner).await?;
    for hour in [start, start + ChronoDuration::hours(1)] {
        sqlx::query("SELECT transaction_events_create_hour('hot',$1)")
            .bind(hour)
            .execute(&mut owner)
            .await?;
    }
    let wrong_hour = start + ChronoDuration::hours(2);
    let name = format!("transaction_events_hot_{}", wrong_hour.format("%Y%m%d%H"));
    let parent = format!("transaction_events_hot_{}", start.format("%Y%m%d"));
    sqlx::query(&format!(
        "CREATE TABLE {name}(LIKE transaction_events INCLUDING DEFAULTS, PRIMARY KEY(event_id))"
    ))
    .execute(&mut owner)
    .await?;
    sqlx::query(&format!(
        "ALTER TABLE {parent} ATTACH PARTITION {name} FOR VALUES FROM ('{}') TO ('{}')",
        wrong_hour + ChronoDuration::hours(1),
        wrong_hour + ChronoDuration::hours(2)
    ))
    .execute(&mut owner)
    .await?;
    sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
    for action in ["create", "detach", "drop_detached"] {
        assert!(
            sqlx::query(&format!("SELECT transaction_events_{action}_hour('hot',$1)"))
                .bind(wrong_hour)
                .execute(&mut db.admin)
                .await
                .is_err()
        );
    }
    sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    assert!(HourlyTransactionEventPartitions::hours(&mut owner).await.is_err());
    assert!(
        HourlyTransactionEventPartitions::horizons(&mut owner, start).await.is_err(),
        "wrong-bound coverage must not appear healthy"
    );
    assert!(HourlyTransactionEventPartitions::validate(&mut owner).await.is_err());
    let attached: bool =
        sqlx::query_scalar("SELECT relispartition FROM pg_class WHERE oid=to_regclass($1)")
            .bind(&name)
            .fetch_one(&mut owner)
            .await?;
    assert!(attached, "must not drop or repurpose malformed existing hour");
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
/// Preparation proves the exact generated CHECK type and predicate.
pub async fn hourly_catalog_forged_preparation_check_is_rejected() -> anyhow::Result<()> {
    let db = HourlyDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut owner = PgConnection::connect(&db.url).await?;
    HourlyDatabase::prepare_hourly(&mut owner).await?;
    let old_day = Utc::now().date_naive();
    let name = format!("transaction_events_hot_{}", old_day.format("%Y%m%d"));
    let sink = PgTransactionEventSink::connect(&db.url, 2).await?;
    sink.insert_events(&[HourlyDatabase::hourly_event(
        "retained-preparation",
        Utc::now(),
        "BUILDER_ACCEPTED",
    )])
    .await?;
    let cutoff = old_day + ChronoDuration::days(7);
    for predicate in [
        "CHECK(true)",
        "CHECK(retention_class='warm') NOT VALID",
        "CHECK(event_date >= '2000-01-01'::date)",
        "UNIQUE(event_id,retention_class,event_date)",
    ] {
        sqlx::raw_sql(&format!("ALTER TABLE {name} DROP CONSTRAINT hourly_day_bound; ALTER TABLE {name} ADD CONSTRAINT hourly_day_bound {predicate}")).execute(&mut owner).await?;
        let oid: i64=sqlx::query_scalar("SELECT oid::bigint FROM pg_constraint WHERE conrelid=to_regclass($1) AND conname='hourly_day_bound'").bind(&name).fetch_one(&mut owner).await?;
        assert!(
            sqlx::query("SELECT transaction_events_prepare_hourly('hot',$1)")
                .bind(old_day)
                .execute(&mut owner)
                .await
                .is_err()
        );
        assert!(HourlyTransactionEventPartitions::activate(&db.url, cutoff, true).await.is_err());
        let after: i64=sqlx::query_scalar("SELECT oid::bigint FROM pg_constraint WHERE conrelid=to_regclass($1) AND conname='hourly_day_bound'").bind(&name).fetch_one(&mut owner).await?;
        assert_eq!(oid, after, "must not silently replace forged proof");
        assert_eq!(HourlyTransactionEventPartitions::cutoff(&mut owner).await?, None);
    }
    sqlx::query(&format!("ALTER TABLE {name} DROP CONSTRAINT hourly_day_bound"))
        .execute(&mut owner)
        .await?;
    sqlx::query("SELECT transaction_events_prepare_hourly('hot',$1)")
        .bind(old_day)
        .execute(&mut owner)
        .await?;
    sqlx::raw_sql("ALTER TABLE transaction_events_hot DROP CONSTRAINT hourly_class_bound; ALTER TABLE transaction_events_hot ADD CONSTRAINT hourly_class_bound CHECK(true)").execute(&mut owner).await?;
    assert!(
        sqlx::query("SELECT transaction_events_prepare_hourly('hot')")
            .execute(&mut owner)
            .await
            .is_err()
    );
    assert!(HourlyTransactionEventPartitions::activate(&db.url, cutoff, true).await.is_err());
    sqlx::query("ALTER TABLE transaction_events_hot DROP CONSTRAINT hourly_class_bound")
        .execute(&mut owner)
        .await?;
    sqlx::query("SELECT transaction_events_prepare_hourly('hot')").execute(&mut owner).await?;
    HourlyTransactionEventPartitions::activate(&db.url, cutoff, true).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM transaction_events WHERE event_id='retained-preparation'",
    )
    .fetch_one(&mut owner)
    .await?;
    assert_eq!(count, 1);
    HourlyTransactionEventPartitions::validate(&mut owner).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
/// Hourly identity, complete reads, clocks, admission and bounded DDL remain compatible.
pub async fn hourly_bridge_preserves_identity_history_reads_and_native_catalog()
-> anyhow::Result<()> {
    let mut db = HourlyDatabase::new().await?;
    // Runtime DML is tested without ownership or direct-leaf grants. Other
    // acceptance fixtures may already have created this role in the owned cluster.
    let runtime_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='audit_archiver')")
            .fetch_one(&mut db.admin)
            .await?;
    if !runtime_exists {
        sqlx::query("CREATE ROLE audit_archiver NOLOGIN").execute(&mut db.admin).await?;
    }
    // Role creation must precede migration: grants are immutable migration SQL.
    PgTransactionEventSink::migrate(&db.url).await?;
    let sink = PgTransactionEventSink::connect(&db.url, 8).await?;
    let mut owner = PgConnection::connect(&db.url).await?;
    assert_eq!(HourlyTransactionEventPartitions::cutoff(&mut owner).await?, None);
    let day = Utc::now().date_naive() + ChronoDuration::days(7);
    let ingest_clock = Utc::now();
    sqlx::query("SELECT transaction_events_create_partition('hot',$1)")
        .bind(day - ChronoDuration::days(1))
        .execute(&mut owner)
        .await?;
    // Synthetic event clock around future T; arrival/ingested_at stays real UTC.
    let old_time = day.and_hms_opt(0, 0, 0).unwrap().and_utc() - ChronoDuration::hours(12);
    let old = HourlyDatabase::hourly_event("opaque old / not UUID", old_time, "BUILDER_ACCEPTED");
    sink.insert_events(std::slice::from_ref(&old)).await?;
    // Capture the root plus the actual populated heap and its indexes.
    sqlx::query("CREATE TEMP TABLE preserved_relations AS SELECT c.oid,c.relfilenode FROM pg_class c WHERE c.oid='transaction_events'::regclass OR c.oid IN (SELECT tableoid FROM transaction_events) OR c.oid IN (SELECT indexrelid FROM pg_index WHERE indrelid IN (SELECT tableoid FROM transaction_events))").execute(&mut owner).await?;
    assert!(HourlyTransactionEventPartitions::activate(&db.url, day, false).await.is_err());
    assert!(HourlyTransactionEventPartitions::activate(&db.url, day, true).await.is_err());
    HourlyDatabase::prepare_hourly(&mut owner).await?;
    // Preparation must not break daily creation while bridge rollout continues.
    for class in ["hot", "warm", "cold"] {
        sqlx::query("SELECT transaction_events_create_partition($1,$2)")
            .bind(class)
            .bind(day - ChronoDuration::days(1))
            .execute(&mut owner)
            .await?;
    }
    for class in ["hot", "warm"] {
        sqlx::query("SELECT transaction_events_prepare_hourly($1,$2)")
            .bind(class)
            .bind(day - ChronoDuration::days(1))
            .execute(&mut owner)
            .await?;
    }
    sqlx::query("SELECT transaction_events_create_partition('hot',$1)")
        .bind(day)
        .execute(&mut owner)
        .await?;
    // A populated future daily leaf must abort instead of being copied/reset.
    let future = HourlyDatabase::hourly_event(
        "future-occupied",
        day.and_hms_opt(0, 0, 0).unwrap().and_utc(),
        "BUILDER_ACCEPTED",
    );
    sink.insert_events(&[future]).await?;
    assert!(HourlyTransactionEventPartitions::activate(&db.url, day, true).await.is_err());
    assert_eq!(HourlyTransactionEventPartitions::cutoff(&mut owner).await?, None);
    sqlx::query("DELETE FROM transaction_events WHERE event_id='future-occupied'")
        .execute(&mut owner)
        .await?;
    // Even an EMPTY overlapping day is refused: activation must not open a gap.
    assert!(HourlyTransactionEventPartitions::activate(&db.url, day, true).await.is_err());
    sqlx::query("SELECT transaction_events_detach_partition('hot',$1)")
        .bind(day)
        .execute(&mut owner)
        .await?;
    sqlx::query("SELECT transaction_events_drop_detached_partition('hot',$1)")
        .bind(day)
        .execute(&mut owner)
        .await?;
    // A root reader forces bounded DDL wait; failed activation is fully atomic.
    let mut reader = PgConnection::connect(&db.url).await?;
    let mut held = reader.begin().await?;
    sqlx::query("SELECT count(*) FROM transaction_events").execute(&mut *held).await?;
    let mut blocked = owner.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='50ms'").execute(&mut *blocked).await?;
    let error = sqlx::query("SELECT transaction_events_activate_hourly($1,true)")
        .bind(day)
        .execute(&mut *blocked)
        .await
        .unwrap_err();
    assert_eq!(error.as_database_error().and_then(|e| e.code()).as_deref(), Some("55P03"));
    blocked.rollback().await?;
    held.commit().await?;
    assert_eq!(HourlyTransactionEventPartitions::cutoff(&mut owner).await?, None);
    HourlyTransactionEventPartitions::activate(&db.url, day, true).await?;
    let unchanged:bool=sqlx::query_scalar("SELECT bool_and(c.oid IS NOT NULL AND c.relfilenode=p.relfilenode) FROM preserved_relations p LEFT JOIN pg_class c USING(oid)").fetch_one(&mut owner).await?;
    assert!(unchanged, "populated history/root/index OIDs and files must survive");
    // Original request-level ID filtering is global across classes/timestamps.
    // Real wall-clock events still route to preserved daily leaves before T.
    let request_time = Utc::now();
    let http_one =
        HourlyDatabase::hourly_event("http opaque / ID", request_time, "BUILDER_ACCEPTED");
    let http_two = HourlyDatabase::hourly_event(
        "http opaque / ID",
        request_time + ChronoDuration::seconds(1),
        "SIMULATION_SUCCEEDED",
    );
    let router = audit_archiver_lib::TransactionEventIngestConfig {
        path: audit_archiver_lib::DEFAULT_TRANSACTION_EVENT_BATCH_PATH.to_string(),
        max_batch_size: audit_archiver_lib::DEFAULT_TRANSACTION_EVENT_MAX_BATCH_SIZE,
        max_event_bytes: audit_archiver_lib::DEFAULT_TRANSACTION_EVENT_MAX_EVENT_BYTES,
        max_data_bytes: audit_archiver_lib::DEFAULT_TRANSACTION_EVENT_MAX_DATA_BYTES,
        max_request_bytes: audit_archiver_lib::DEFAULT_TRANSACTION_EVENT_MAX_REQUEST_BYTES,
    }
    .into_router(Arc::new(PgTransactionEventSink::connect(&db.url, 2).await?));
    let body =
        format!("{}\n{}\n", serde_json::to_string(&http_one)?, serde_json::to_string(&http_two)?);
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(audit_archiver_lib::DEFAULT_TRANSACTION_EVENT_BATCH_PATH)
                .header("content-type", "application/x-ndjson")
                .body(Body::from(body))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let counts: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await?)?;
    assert_eq!(counts["accepted"], 1);
    assert_eq!(counts["duplicate"], 1);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM transaction_events WHERE event_id='http opaque / ID'",
    )
    .fetch_one(&mut owner)
    .await?;
    assert_eq!(rows, 1);
    let start = day.and_hms_opt(0, 0, 0).unwrap().and_utc();
    sink.maintain_partitions_at(start + ChronoDuration::hours(12)).await?;
    let id = "any nonempty ID: / ' spaces ☃";
    let first =
        HourlyDatabase::hourly_event(id, start + ChronoDuration::minutes(1), "BUILDER_ACCEPTED");
    let changed =
        HourlyDatabase::hourly_event(id, start + ChronoDuration::minutes(59), "BUILDER_ACCEPTED");
    assert_eq!(sink.insert_events(std::slice::from_ref(&first)).await?.inserted_event_ids.len(), 1);
    assert!(sink.insert_events(&[changed]).await?.inserted_event_ids.is_empty());
    // Retry after a lost success acknowledgement keeps the original timestamps.
    assert!(sink.insert_events(std::slice::from_ref(&first)).await?.inserted_event_ids.is_empty());
    let other =
        HourlyDatabase::hourly_event(id, start + ChronoDuration::hours(1), "BUILDER_ACCEPTED");
    assert_eq!(sink.insert_events(std::slice::from_ref(&other)).await?.inserted_event_ids.len(), 1);
    let next =
        HourlyDatabase::hourly_event(id, start + ChronoDuration::days(1), "BUILDER_ACCEPTED");
    sink.insert_events(std::slice::from_ref(&next)).await?;
    let cold = HourlyDatabase::hourly_event("cold-ID", start, "SIMULATION_FAILED");
    sink.insert_events(std::slice::from_ref(&cold)).await?;
    assert!(
        sink.insert_events(&[HourlyDatabase::hourly_event(
            "cold-ID",
            start + ChronoDuration::hours(1),
            "SIMULATION_FAILED"
        )])
        .await?
        .inserted_event_ids
        .is_empty()
    );
    let warm = HourlyDatabase::hourly_event(id, start, "SIMULATION_SUCCEEDED");
    let cold_next = HourlyDatabase::hourly_event(
        "cold-ID",
        start + ChronoDuration::days(1),
        "SIMULATION_FAILED",
    );
    assert_eq!(sink.insert_events(&[cold_next]).await?.inserted_event_ids.len(), 1);
    sink.insert_events(std::slice::from_ref(&warm)).await?;
    let mut old_retry = old.clone();
    old_retry.event_time = old_time + ChronoDuration::seconds(1);
    assert!(sink.insert_events(&[old_retry]).await?.inserted_event_ids.is_empty());
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM transaction_events WHERE event_id=$1")
            .bind(id)
            .fetch_one(&mut owner)
            .await?;
    assert_eq!(count, 4);
    let times:Vec<chrono::DateTime<Utc>>=sqlx::query_scalar("SELECT event_time FROM transaction_events WHERE event_id=$1 AND retention_class='hot' ORDER BY event_time").bind(id).fetch_all(&mut owner).await?;
    assert_eq!(times, vec![first.event_time, other.event_time, next.event_time]);
    let tx = sink
        .events_by_transaction_hash(
            "0x1111111111111111111111111111111111111111111111111111111111111111",
            2000,
        )
        .await?;
    assert_eq!(tx.len(), 7);
    assert!(tx.windows(2).all(|w| (w[0].event.event_time, w[0].ingested_at, &w[0].event.event_id)
        <= (w[1].event.event_time, w[1].ingested_at, &w[1].event.event_id)));
    assert_eq!(sink.events_by_bundle("hourly-bundle", 2000).await?.len(), 3);
    assert_eq!(sink.events_by_block_number(123, 2000).await?.len(), 7);
    // ETL must find a freshly ingested row in a pre-cutover event-time bucket.
    let late = HourlyDatabase::hourly_event("late-old-bucket", old_time, "BUILDER_ACCEPTED");
    assert!(sink.admit_event(&late, start + ChronoDuration::hours(12)).is_ok());
    sink.insert_events(&[late]).await?;
    let found:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM transaction_events WHERE event_id='late-old-bucket' AND ingested_at>= $1)").bind(ingest_clock).fetch_one(&mut owner).await?;
    assert!(found);
    // Concurrent conflicting inserts retain exactly one first winner.
    let concurrent = HourlyDatabase::hourly_event(
        "concurrent",
        start + ChronoDuration::minutes(2),
        "BUILDER_ACCEPTED",
    );
    let one = [concurrent.clone()];
    let two = [concurrent.clone()];
    let (a, b) = tokio::join!(sink.insert_events(&one), sink.insert_events(&two));
    assert_eq!(a?.inserted_event_ids.len() + b?.inserted_event_ids.len(), 1);
    // A known rollback must not poison the same leaf's identity for the retry.
    let mut rollback = owner.begin().await?;
    sqlx::query("INSERT INTO transaction_events (event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES ('rolled-back','transaction-event/v1',$1,$2,'hot','base-builder','BUILDER_ACCEPTED','{}') ON CONFLICT DO NOTHING").bind(start).bind(day).execute(&mut *rollback).await?;
    rollback.rollback().await?;
    sink.insert_events(&[HourlyDatabase::hourly_event("rolled-back", start, "BUILDER_ACCEPTED")])
        .await?;
    // Admission uses event_time, not arrival time; exact TTL equality accepted.
    assert!(
        sink.admit_event(&first, start + ChronoDuration::minutes(1) + ChronoDuration::days(3))
            .is_ok()
    );
    assert!(
        sink.admit_event(
            &first,
            start
                + ChronoDuration::minutes(1)
                + ChronoDuration::days(3)
                + ChronoDuration::nanoseconds(1)
        )
        .is_err()
    );
    assert!(sink.admit_event(&other, start).is_err(), "exact +1h future skew rejected");
    assert!(sink.admit_event(&first, start).is_ok());
    let horizons = HourlyTransactionEventPartitions::horizons(&mut owner, start).await?;
    assert!(horizons.iter().all(|(_, seconds)| *seconds > 0.0));
    // A missing future hour must lower coverage despite its day branch existing.
    let gap = start + ChronoDuration::hours(2);
    sqlx::query("SELECT transaction_events_detach_hour('hot',$1)")
        .bind(gap)
        .execute(&mut owner)
        .await?;
    let gap_horizon = HourlyTransactionEventPartitions::horizons(&mut owner, start).await?;
    assert_eq!(gap_horizon.iter().find(|(c, _)| *c == "hot").unwrap().1, 3600.0);
    sink.maintain_partitions_at(start).await?;
    assert!(
        HourlyTransactionEventPartitions::horizons(&mut owner, start)
            .await?
            .iter()
            .all(|(_, seconds)| *seconds > 3600.0)
    );
    let hour_table = format!("transaction_events_hot_{}", other.event_time.format("%Y%m%d%H"));
    let direct: bool =
        sqlx::query_scalar("SELECT has_table_privilege('audit_archiver',$1,'INSERT')")
            .bind(&hour_table)
            .fetch_one(&mut owner)
            .await?;
    assert!(!direct, "bridge must not need direct-leaf INSERT grants");
    sqlx::query("SET ROLE audit_archiver").execute(&mut db.admin).await?;
    for (minute, expected) in [(10, 1), (11, 0)] {
        let ids:Vec<String>=sqlx::query_scalar("INSERT INTO transaction_events (event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES ('runtime-root','transaction-event/v1',$1,$2,'hot','base-builder','BUILDER_ACCEPTED','{}') ON CONFLICT DO NOTHING RETURNING event_id").bind(other.event_time+ChronoDuration::minutes(minute)).bind(day).fetch_all(&mut db.admin).await?;
        assert_eq!(ids.len(), expected);
    }
    sqlx::query("RESET ROLE").execute(&mut db.admin).await?;
    // Read-only restoration rejects a lost local dedupe arbiter as well as BRIN.
    sqlx::query(&format!("ALTER TABLE {hour_table} DROP CONSTRAINT {hour_table}_pkey"))
        .execute(&mut owner)
        .await?;
    assert!(HourlyTransactionEventPartitions::validate(&mut owner).await.is_err());
    let absent: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM pg_index WHERE indrelid=to_regclass($1) AND indisprimary)",
    )
    .bind(&hour_table)
    .fetch_one(&mut owner)
    .await?;
    assert!(absent, "read-only validation must not repair PK state");
    sqlx::query(&format!("ALTER TABLE {hour_table} ADD PRIMARY KEY(event_id)"))
        .execute(&mut owner)
        .await?;
    // Expiry at the exclusive end-hour plus TTL/grace; never recreate expired hours.
    sink.maintain_partitions_at(start + ChronoDuration::days(3) + ChronoDuration::hours(2)).await?;
    let expired: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NULL")
        .bind(format!("public.transaction_events_hot_{}", start.format("%Y%m%d%H")))
        .fetch_one(&mut owner)
        .await?;
    assert!(expired);
    let remaining: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM transaction_events WHERE event_id=$1 AND event_time=$2)",
    )
    .bind(id)
    .bind(other.event_time)
    .fetch_one(&mut owner)
    .await?;
    assert!(remaining);
    sink.maintain_partitions_at(start + ChronoDuration::days(3) + ChronoDuration::hours(2)).await?;
    let absent: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM transaction_events WHERE event_id=$1 AND event_time=$2)",
    )
    .bind(id)
    .bind(first.event_time)
    .fetch_one(&mut owner)
    .await?;
    assert!(absent);
    // A reader can hold an already-detached expired DAY. Failed day unlink
    // must not misclassify its hour children or prevent fresh hour coverage.
    let after_day_expiry = start + ChronoDuration::days(4) + ChronoDuration::hours(1);
    sqlx::query("SELECT transaction_events_detach_partition('hot',$1)")
        .bind(day)
        .execute(&mut owner)
        .await?;
    let detached_day = format!("transaction_events_hot_{}", day.format("%Y%m%d"));
    let mut held_day = owner.begin().await?;
    sqlx::query(&format!("SELECT 1 FROM ONLY {detached_day} LIMIT 1"))
        .execute(&mut *held_day)
        .await?;
    let short = PgTransactionEventSink::connect(&db.url, 2).await?.with_retention_config(
        audit_archiver_lib::TransactionEventRetentionConfig {
            partition_lock_timeout_ms: 50,
            ..Default::default()
        },
    )?;
    let deferred = short.maintain_partitions_at(after_day_expiry).await?;
    assert!(deferred.lock_timeouts > 0);
    let coverage =
        HourlyTransactionEventPartitions::horizons(&mut db.admin, after_day_expiry).await?;
    assert!(coverage.iter().all(|(_, seconds)| *seconds > 3.0 * 86400.0));
    held_day.commit().await?;
    short.maintain_partitions_at(after_day_expiry).await?;
    let removed: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NULL")
        .bind(&detached_day)
        .fetch_one(&mut owner)
        .await?;
    assert!(removed);
    HourlyTransactionEventPartitions::validate(&mut owner).await?;
    HourlyTransactionEventPartitions::validate_indexes(&mut owner, false).await?;
    let allowed:bool=sqlx::query_scalar("SELECT has_function_privilege('audit_archiver','public.transaction_events_activate_hourly(date,boolean)','EXECUTE')").fetch_one(&mut owner).await?;
    assert!(!allowed);
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
/// A lost COMMIT acknowledgement cannot change event-hour identity or first winner.
pub async fn hourly_unknown_commit_replay_keeps_original_event_bucket() -> anyhow::Result<()> {
    let mut db = HourlyDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut owner = PgConnection::connect(&db.url).await?;
    HourlyDatabase::prepare_hourly(&mut owner).await?;
    let day = Utc::now().date_naive() + ChronoDuration::days(7);
    HourlyTransactionEventPartitions::activate(&db.url, day, true).await?;
    let start = day.and_hms_opt(0, 0, 0).unwrap().and_utc();
    for hour in [start, start + ChronoDuration::hours(1)] {
        sqlx::query("SELECT transaction_events_create_hour('hot',$1)")
            .bind(hour)
            .execute(&mut owner)
            .await?;
    }
    let event_time = start + ChronoDuration::minutes(59);
    let proxy = ResponseProxy::start(&db.url, b"COMMIT").await?;
    let mut conn = PgConnection::connect(&proxy.url).await?;
    let (pid, backend_start): (i32, chrono::DateTime<Utc>) =
        sqlx::query_as("SELECT pid,backend_start FROM pg_stat_activity WHERE pid=pg_backend_pid()")
            .fetch_one(&mut conn)
            .await?;
    let pending = tokio::spawn(async move {
        let mut tx = conn.begin().await?;
        sqlx::query(r#"INSERT INTO transaction_events (event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES ('opaque unknown / commit','transaction-event/v1',$1,$2,'hot','base-builder','BUILDER_ACCEPTED','{"first":true}') ON CONFLICT DO NOTHING"#).bind(event_time).bind(day).execute(&mut *tx).await?;
        tx.commit().await?;
        anyhow::Ok(())
    });
    timeout(Duration::from_secs(5), proxy.triggered.notified()).await?;
    db.wait(
        "SELECT EXISTS(SELECT 1 FROM transaction_events WHERE event_id='opaque unknown / commit')",
    )
    .await?;
    let before:(chrono::DateTime<Utc>,chrono::DateTime<Utc>,serde_json::Value)=sqlx::query_as("SELECT event_time,ingested_at,data FROM transaction_events WHERE event_id='opaque unknown / commit'").fetch_one(&mut owner).await?;
    assert!(!pending.is_finished(), "success response was actually withheld");
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    drop(proxy);
    // The canceled unpooled client closes its own socket. Verify exact backend
    // absence before continuing; never signal unrelated fixture sessions.
    timeout(Duration::from_secs(5), async {
        while sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND backend_start=$2)",
        )
        .bind(pid)
        .bind(backend_start)
        .fetch_one(&mut owner)
        .await?
        {
            sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    let sink = PgTransactionEventSink::connect(&db.url, 2).await?;
    let retry =
        HourlyDatabase::hourly_event("opaque unknown / commit", event_time, "BUILDER_ACCEPTED");
    assert!(
        sink.admit_event(&retry, start + ChronoDuration::hours(1) + ChronoDuration::minutes(1))
            .is_ok()
    );
    assert!(sink.insert_events(&[retry]).await?.inserted_event_ids.is_empty());
    let after:Vec<(chrono::DateTime<Utc>,chrono::DateTime<Utc>,serde_json::Value)>=sqlx::query_as("SELECT event_time,ingested_at,data FROM transaction_events WHERE event_id='opaque unknown / commit'").fetch_all(&mut owner).await?;
    assert_eq!(after, vec![before]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicitly owned disposable PostgreSQL 17"]
/// Full class/day/hour ancestry gates reads and manual index repair.
pub async fn hourly_catalog_swapped_hour_names_are_rejected() -> anyhow::Result<()> {
    let mut db = HourlyDatabase::new().await?;
    let (mut owner, hour) = db.catalog_hourly().await?;
    for class in ["hot", "warm"] {
        sqlx::query("SELECT transaction_events_create_hour($1,$2)")
            .bind(class)
            .bind(hour)
            .execute(&mut owner)
            .await?;
    }
    let hot = format!("transaction_events_hot_{}", hour.format("%Y%m%d%H"));
    let warm = format!("transaction_events_warm_{}", hour.format("%Y%m%d%H"));
    sqlx::raw_sql(&format!("ALTER TABLE {hot} RENAME TO swap_hour; ALTER TABLE {warm} RENAME TO {hot}; ALTER TABLE swap_hour RENAME TO {warm}")).execute(&mut owner).await?;
    assert!(
        HourlyTransactionEventPartitions::validate(&mut owner).await.is_err(),
        "swapped class/hour names accepted by schema validator"
    );
    assert!(
        HourlyTransactionEventPartitions::validate_indexes(&mut owner, false).await.is_err(),
        "swapped ancestry accepted by BRIN validator"
    );
    let before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class WHERE relnamespace='public'::regnamespace",
    )
    .fetch_one(&mut owner)
    .await?;
    assert!(index_transaction_event_partitions(&db.url).await.is_err());
    let after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class WHERE relnamespace='public'::regnamespace",
    )
    .fetch_one(&mut owner)
    .await?;
    assert_eq!(before, after, "malformed ancestry must not start index repair");
    // Restore the hours, then swap whole day names while keeping valid edges.
    sqlx::raw_sql(&format!("ALTER TABLE {hot} RENAME TO swap_hour; ALTER TABLE {warm} RENAME TO {hot}; ALTER TABLE swap_hour RENAME TO {warm}")).execute(&mut owner).await?;
    let hot_day = format!("transaction_events_hot_{}", hour.format("%Y%m%d"));
    let warm_day = format!("transaction_events_warm_{}", hour.format("%Y%m%d"));
    sqlx::raw_sql(&format!("ALTER TABLE {hot_day} RENAME TO swap_day; ALTER TABLE {warm_day} RENAME TO {hot_day}; ALTER TABLE swap_day RENAME TO {warm_day}")).execute(&mut owner).await?;
    assert!(HourlyTransactionEventPartitions::validate(&mut owner).await.is_err());
    Ok(())
}
