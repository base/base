//! Online, resumable indexing of existing transaction-event day partitions.

use anyhow::{Context, Result, bail, ensure};
use sqlx::{migrate::Migrate, postgres::PgPoolOptions};
use tracing::{info, warn};

/// Builds BRIN indexes on populated day partitions without blocking inserts.
///
/// `migrate up` must first install the parent-only index definitions. This
/// operation is separate from migrations so the chart's migrator init container
/// does not wait for every production day partition to be scanned. The sqlx
/// migration lock prevents two operators from building the same leaf. Failed
/// concurrent builds leave an invalid index; a later invocation drops and
/// rebuilds that leaf before resuming.
pub async fn index_transaction_event_partitions(database_url: &str) -> Result<usize> {
    let pool = PgPoolOptions::new().max_connections(1).connect(database_url).await?;
    let mut conn = pool.acquire().await?;
    conn.lock().await?;
    let result = index_partitions(&mut conn).await;
    if let Err(err) = conn.unlock().await {
        warn!(error = %err, "failed to release transaction event index migration lock");
    }
    result
}

async fn index_partitions(conn: &mut sqlx::PgConnection) -> Result<usize> {
    let ready: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = 2 AND success) \
         AND to_regclass('public.transaction_events_ingested_at_idx') IS NOT NULL",
    )
    .fetch_one(&mut *conn)
    .await?;
    ensure!(ready, "run audit-archiver migrate up before indexing transaction event partitions");

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

        for (class, leaf) in leaves {
            let day = leaf.strip_prefix(&format!("{class}_")).unwrap_or_default();
            ensure!(
                day.len() == 8 && day.bytes().all(|byte| byte.is_ascii_digit()),
                "unexpected transaction event day partition name: {leaf}"
            );
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
                }
                Some((true, Some(parent))) if parent == &class_index => continue,
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

    bail!("transaction event ingested_at index is still invalid; retry after partition maintenance")
}
