//! Online, resumable indexing of existing transaction-event day partitions.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sqlx::{migrate::Migrate, postgres::PgPoolOptions};
use tracing::{info, warn};

/// `lock_timeout` for one leaf index ATTACH.
///
/// ATTACH takes an ACCESS EXCLUSIVE lock on the leaf index. Reads that do not
/// prune by `event_date` hold ACCESS SHARE on every leaf index for their whole
/// run, and new ones queue behind a waiting ATTACH. Inserts only lock the day
/// they write to, so they are not held up.
const ATTACH_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Total time spent retrying attaches that hit [`ATTACH_LOCK_TIMEOUT`].
const ATTACH_RETRY_BUDGET: Duration = Duration::from_secs(3_600);

/// First pause before retrying a deferred attach; doubles each round.
const ATTACH_RETRY_INITIAL_DELAY: Duration = Duration::from_secs(5);

/// Longest pause before retrying a deferred attach.
const ATTACH_RETRY_MAX_DELAY: Duration = Duration::from_secs(60);

/// SQLSTATE `lock_not_available`, raised when `lock_timeout` expires.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// Builds BRIN indexes on populated day partitions without blocking inserts.
///
/// `migrate up` must first install the parent-only index definitions. This
/// operation is separate from migrations so the chart's migrator init container
/// does not wait for every production day partition to be scanned. The sqlx
/// migration lock prevents two operators from building the same leaf. Failed
/// concurrent builds leave an invalid index; a later invocation drops and
/// rebuilds that leaf before resuming.
///
/// An ATTACH that hits its lock timeout does not stop the run: remaining
/// leaves are built first, then pending attaches are retried with backoff for
/// up to [`ATTACH_RETRY_BUDGET`].
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

/// A built leaf index that still needs to be attached to its class index.
struct PendingAttach {
    leaf: String,
    class_index: String,
    name: String,
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

        let mut pending = Vec::new();
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

            let attach = PendingAttach { leaf, class_index, name };
            if !try_attach(conn, &attach).await? {
                warn!(leaf = %attach.leaf, "deferring transaction event day index attach");
                pending.push(attach);
            }
        }

        let deadline = Instant::now() + ATTACH_RETRY_BUDGET;
        let mut delay = ATTACH_RETRY_INITIAL_DELAY;
        while !pending.is_empty() {
            if Instant::now() >= deadline {
                let leaves: Vec<_> = pending.iter().map(|attach| attach.leaf.as_str()).collect();
                bail!(
                    "attaching transaction event day indexes timed out on {}; retry the index \
                     command",
                    leaves.join(", ")
                );
            }
            let mut still_pending = Vec::new();
            for attach in pending {
                // Pause before every attempt, not every round: back-to-back
                // waits would keep unpruned reads queued almost continuously.
                tokio::time::sleep(delay).await;
                if try_attach(conn, &attach).await? {
                    info!(leaf = %attach.leaf, "attached deferred transaction event day index");
                } else {
                    still_pending.push(attach);
                }
            }
            pending = still_pending;
            delay = (delay * 2).min(ATTACH_RETRY_MAX_DELAY);
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

/// Attaches one leaf index, returning `false` if the lock wait timed out.
async fn try_attach(conn: &mut sqlx::PgConnection, attach: &PendingAttach) -> Result<bool> {
    let PendingAttach { leaf, class_index, name } = attach;
    sqlx::query(&format!("SET lock_timeout = {}", ATTACH_LOCK_TIMEOUT.as_millis()))
        .execute(&mut *conn)
        .await?;
    let result =
        sqlx::query(&format!("ALTER INDEX public.{class_index} ATTACH PARTITION public.{name}"))
            .execute(&mut *conn)
            .await;
    sqlx::query("SET lock_timeout = 0").execute(&mut *conn).await?;

    match result {
        Ok(_) => Ok(true),
        Err(err)
            if err.as_database_error().and_then(|error| error.code()).as_deref()
                == Some(LOCK_NOT_AVAILABLE) =>
        {
            Ok(false)
        }
        Err(err) => Err(err)
            .with_context(|| format!("attaching index for transaction event day partition {leaf}")),
    }
}

#[cfg(test)]
mod tests {
    use sqlx::{Executor, PgPool};
    use testcontainers::{ImageExt, runners::AsyncRunner};
    use testcontainers_modules::postgres::Postgres;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::PgTransactionEventSink;

    /// Starts a read that holds ACCESS SHARE on `leaf`'s indexes until it is
    /// canceled or `sleep_secs` pass, and waits until it holds them.
    async fn start_read_on(
        pool: &PgPool,
        leaf: &str,
        sleep_secs: u32,
    ) -> Result<JoinHandle<Result<(), sqlx::Error>>> {
        let reader = pool.clone();
        let query = format!(
            "SELECT pg_sleep({sleep_secs}), \
             (SELECT count(*) FROM public.{leaf} WHERE tx_hash = 'blocking-read')"
        );
        let read =
            tokio::spawn(async move { sqlx::query(&query).execute(&reader).await.map(|_| ()) });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let locked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks l \
                 JOIN pg_stat_activity a ON a.pid = l.pid \
                 WHERE a.query LIKE 'SELECT pg_sleep(%' AND l.relation = to_regclass($1))",
            )
            .bind(format!("public.{leaf}_ingested_at_idx"))
            .fetch_one(pool)
            .await?;
            if locked {
                return Ok(read);
            }
            ensure!(Instant::now() < deadline, "read never locked {leaf}'s indexes");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Leaf indexes not yet attached to their class index.
    async fn unattached_leaf_indexes(pool: &PgPool) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_class c \
             WHERE c.relkind = 'i' AND c.relname LIKE 'transaction_events_%_ingested_at_idx' \
               AND c.relname NOT IN ('transaction_events_hot_ingested_at_idx', \
                   'transaction_events_warm_ingested_at_idx', \
                   'transaction_events_cold_ingested_at_idx') \
               AND NOT EXISTS (SELECT 1 FROM pg_inherits i WHERE i.inhrelid = c.oid) \
             ORDER BY 1",
        )
        .fetch_all(pool)
        .await?)
    }

    #[tokio::test]
    async fn defers_attach_blocked_by_long_read() -> Result<()> {
        let container = Postgres::default().with_tag("17-alpine").start().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let database_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        PgTransactionEventSink::migrate(&database_url).await?;
        let pool = PgPoolOptions::new().max_connections(4).connect(&database_url).await?;

        // Build the missing leaf indexes up front so the command only
        // attaches. CREATE INDEX CONCURRENTLY would otherwise wait out the
        // blocking read.
        let leaves: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_inherits i \
             JOIN pg_class c ON c.oid = i.inhrelid \
             JOIN pg_class p ON p.oid = i.inhparent \
             WHERE p.relname IN \
                 ('transaction_events_hot', 'transaction_events_warm', 'transaction_events_cold') \
               AND to_regclass('public.' || c.relname || '_ingested_at_idx') IS NULL \
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await?;
        ensure!(leaves.len() > 1, "migrations leave several day partitions to index");
        for leaf in &leaves {
            pool.execute(
                format!(
                    "CREATE INDEX {leaf}_ingested_at_idx ON public.{leaf} USING brin (ingested_at)"
                )
                .as_str(),
            )
            .await?;
        }
        // The first leaf in build order, so every later leaf is attached after
        // its attach times out.
        let blocked = &leaves[0];

        // Outlasts one ATTACH_LOCK_TIMEOUT but ends before the first retry
        // finishes waiting.
        let read_secs = ATTACH_LOCK_TIMEOUT.as_secs() as u32 + 15;
        let read = start_read_on(&pool, blocked, read_secs).await?;
        let index = tokio::spawn({
            let database_url = database_url.clone();
            async move { index_transaction_event_partitions(&database_url).await }
        });

        let deadline = Instant::now() + ATTACH_LOCK_TIMEOUT + Duration::from_secs(10);
        loop {
            if unattached_leaf_indexes(&pool).await? == [format!("{blocked}_ingested_at_idx")] {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "later leaves were not attached past the blocked one"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(!read.is_finished(), "later leaves were attached while the read still ran");

        index.await??;
        read.await?.context("the blocking read was not canceled")?;
        assert!(unattached_leaf_indexes(&pool).await?.is_empty());
        let valid: bool = sqlx::query_scalar(
            "SELECT indisvalid FROM pg_index \
             WHERE indexrelid = 'transaction_events_ingested_at_idx'::regclass",
        )
        .fetch_one(&pool)
        .await?;
        assert!(valid, "the root index is valid once every leaf is attached");

        Ok(())
    }
}
