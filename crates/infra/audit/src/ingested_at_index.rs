//! Online, resumable indexing of existing transaction-event day partitions.

use anyhow::{Context, Result, bail, ensure};
use sqlx::{Connection, PgConnection, migrate::Migrate};
use tracing::info;

use crate::{Metrics, MigrationReporter, MigrationSession};

/// Builds BRIN indexes on populated day partitions without blocking inserts.
///
/// This compatibility entry point applies no schema. Ordinary `migrate up`
/// now performs this same reconciliation before reporting success. Both paths
/// serialize on the same migration lock and repair interrupted unattached leaves.
pub async fn index_transaction_event_partitions(database_url: &str) -> Result<usize> {
    let mut conn = MigrationSession::connect(database_url, "audit-index").await?;
    conn.lock().await?;
    let progress = MigrationReporter::new("index".into());
    let result = TransactionEventIngestedAtIndex::reconcile(&mut conn, &progress).await;
    let result = match result {
        Ok(created) => TransactionEventIngestedAtIndex::validate(&mut conn).await.map(|()| created),
        Err(error) => Err(error),
    };
    let unlock = conn.unlock().await;
    let close = conn.close().await;
    let created = result?;
    unlock?;
    close?;
    Ok(created)
}

/// Shared online index reconciliation under an externally owned migration lock.
#[derive(Debug, Clone, Copy)]
pub struct TransactionEventIngestedAtIndex;

impl TransactionEventIngestedAtIndex {
    /// Reconciles one leaf at a time outside transactions, retaining catalog-based resume.
    pub async fn reconcile(conn: &mut PgConnection, progress: &MigrationReporter) -> Result<usize> {
        let ready: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = 2 AND success) \
         AND to_regclass('public.transaction_events_ingested_at_idx') IS NOT NULL",
        )
        .fetch_one(&mut *conn)
        .await?;
        ensure!(
            ready,
            "run audit-archiver migrate up before indexing transaction event partitions"
        );

        // CREATE INDEX CONCURRENTLY cannot run inside a transaction. Keep all
        // statements on this session, with a short lock wait for partition ATTACH
        // and no statement timeout for a legitimately large leaf index build.
        sqlx::query("SET lock_timeout = 0").execute(&mut *conn).await?;
        sqlx::query("SET statement_timeout = 0").execute(&mut *conn).await?;

        let mut created = 0;
        // Maintenance can attach a new day during a long build. Re-enumerate until
        // all three class indexes (and therefore the root) are valid.
        for _ in 0..3 {
            let leaves: Vec<(String, String)> = sqlx::query_as(
            "SELECT parent.relname::text, child.relname::text \
             FROM pg_inherits p \
             JOIN pg_class parent ON parent.oid = p.inhparent \
             JOIN pg_class child ON child.oid = p.inhrelid \
             WHERE parent.relnamespace = 'public'::regnamespace \
               AND child.relnamespace = 'public'::regnamespace \
               AND parent.relname IN \
                   ('transaction_events_hot', 'transaction_events_warm', 'transaction_events_cold') \
               AND child.relkind = 'r' \
             ORDER BY child.relname",
        )
        .fetch_all(&mut *conn)
        .await?;

            progress.update(|s| {
                s.leaves_total = leaves.len() as u64;
                s.leaves_completed = 0;
            })?;
            for (class, leaf) in leaves {
                let day = leaf.strip_prefix(&format!("{class}_")).unwrap_or_default();
                ensure!(
                    day.len() == 8 && day.bytes().all(|byte| byte.is_ascii_digit()),
                    "unexpected transaction event day partition name: {leaf}"
                );
                progress.check_running()?;
                progress.update(|s| s.partition = Some(leaf.clone()))?;
                let name = format!("{leaf}_ingested_at_idx");
                let class_index = format!("{class}_ingested_at_idx");
                let status: Option<(bool, Option<String>)> = sqlx::query_as(
                    "SELECT i.indisvalid, parent.relname::text \
                 FROM pg_class idx \
                 JOIN pg_index i ON i.indexrelid = idx.oid \
                 LEFT JOIN pg_inherits p ON p.inhrelid = idx.oid \
                 LEFT JOIN pg_class parent ON parent.oid = p.inhparent \
                 WHERE idx.oid = to_regclass($1)",
                )
                .bind(format!("public.{name}"))
                .fetch_optional(&mut *conn)
                .await?;

                match status.as_ref() {
                    Some((false, Some(parent))) => {
                        bail!(
                            "invalid attached index {name} under {parent}; repair it before retrying"
                        );
                    }
                    Some((false, None)) => {
                        // A canceled concurrent build leaves an INVALID index with
                        // the intended name. IF NOT EXISTS would silently keep it.
                        sqlx::query(&format!("DROP INDEX CONCURRENTLY public.{name}"))
                            .execute(&mut *conn)
                            .await
                            .with_context(|| format!("dropping invalid index on {leaf}"))?;
                        info!(%leaf, "removed invalid transaction event day index");
                        Metrics::migration_leaves_repaired().increment(1);
                        progress.update(|s| s.leaves_repaired += 1)?;
                    }
                    Some((true, Some(parent))) if parent == &class_index => {
                        Metrics::migration_leaves_skipped().increment(1);
                        progress.update(|s| {
                            s.leaves_skipped += 1;
                            s.leaves_completed += 1;
                        })?;
                        continue;
                    }
                    Some((true, Some(parent))) => {
                        bail!("index {name} is attached to unexpected parent {parent}");
                    }
                    _ => {}
                }

                if !matches!(status, Some((true, None))) {
                    sqlx::query(&format!(
                        "CREATE INDEX CONCURRENTLY {name} ON public.{leaf} USING brin (ingested_at)"
                    ))
                    .execute(&mut *conn)
                    .await
                    .with_context(|| format!("indexing transaction event day partition {leaf}"))?;
                    created += 1;
                    Metrics::migration_leaves_built().increment(1);
                    progress.update(|s| s.leaves_built += 1)?;
                    info!(%leaf, "built transaction event day ingested_at index");
                }

                sqlx::query("SET lock_timeout = '5s'").execute(&mut *conn).await?;
                sqlx::query(&format!(
                    "ALTER INDEX public.{class_index} ATTACH PARTITION public.{name}"
                ))
                .execute(&mut *conn)
                .await
                .with_context(|| {
                    format!("attaching index for transaction event day partition {leaf}")
                })?;
                sqlx::query("SET lock_timeout = 0").execute(&mut *conn).await?;
                progress.update(|s| s.leaves_completed += 1)?;
            }

            let valid: bool = sqlx::query_scalar(
                "SELECT indisvalid FROM pg_index \
             WHERE indexrelid = 'public.transaction_events_ingested_at_idx'::regclass",
            )
            .fetch_one(&mut *conn)
            .await?;
            if valid {
                info!(created, "transaction event ingested_at index is valid on all partitions");
                return Ok(created);
            }
        }

        bail!(
            "transaction event ingested_at index is still invalid; retry after partition maintenance"
        )
    }

    /// Validates every table/index edge and BRIN definition in one catalog snapshot.
    pub async fn validate(conn: &mut PgConnection) -> Result<()> {
        let parents: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relnamespace='public'::regnamespace AND c.relname IN ('transaction_events_ingested_at_idx','transaction_events_hot_ingested_at_idx','transaction_events_warm_ingested_at_idx','transaction_events_cold_ingested_at_idx') AND i.indisvalid AND i.indisready AND i.indrelid=to_regclass('public.' || replace(c.relname,'_ingested_at_idx','')) AND (c.relname='transaction_events_ingested_at_idx' OR EXISTS (SELECT 1 FROM pg_inherits p WHERE p.inhrelid=c.oid AND p.inhparent='public.transaction_events_ingested_at_idx'::regclass))"
    ).fetch_one(&mut *conn).await?;
        ensure!(parents == 4, "required parent indexes incomplete");
        let valid: bool = sqlx::query_scalar(
        "WITH tables AS (SELECT * FROM pg_partition_tree('public.transaction_events'::regclass)), indexes AS (SELECT * FROM pg_partition_tree('public.transaction_events_ingested_at_idx'::regclass)) SELECT NOT EXISTS (SELECT 1 FROM tables t WHERE NOT EXISTS (SELECT 1 FROM indexes x JOIN pg_index i ON i.indexrelid=x.relid JOIN pg_class c ON c.oid=i.indexrelid JOIN pg_am am ON am.oid=c.relam JOIN pg_attribute a ON a.attrelid=t.relid AND a.attname='ingested_at' WHERE i.indrelid=t.relid AND i.indisvalid AND i.indisready AND am.amname='brin' AND i.indnkeyatts=1 AND i.indnatts=1 AND i.indkey[0]=a.attnum AND i.indpred IS NULL AND i.indexprs IS NULL AND (t.parentrelid IS NULL AND x.parentrelid IS NULL OR EXISTS (SELECT 1 FROM pg_index p WHERE p.indexrelid=x.parentrelid AND p.indrelid=t.parentrelid))))"
    ).fetch_one(conn).await?;
        ensure!(valid, "required ingested_at index catalog invalid or incomplete");
        Ok(())
    }
}
