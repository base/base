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

/// `lock_timeout` for an ATTACH that completes its class index.
///
/// Completing a class index makes Postgres validate the root index too, which
/// takes ACCESS EXCLUSIVE on the root table and root index. That attach takes
/// the root table lock first; every insert and read queues behind the wait, so
/// it is kept short.
const ROOT_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// SQLSTATE `lock_not_available`, raised when `lock_timeout` expires.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// SQLSTATE `deadlock_detected`.
const DEADLOCK_DETECTED: &str = "40P01";

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

impl PendingAttach {
    fn statement(&self) -> String {
        format!("ALTER INDEX public.{} ATTACH PARTITION public.{}", self.class_index, self.name)
    }
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

/// Attaches one leaf index, returning `false` if a lock wait timed out or
/// Postgres broke a deadlock by aborting the attach.
async fn try_attach(conn: &mut sqlx::PgConnection, attach: &PendingAttach) -> Result<bool> {
    let result = if completes_class_index(conn, &attach.class_index).await? {
        attach_under_root_lock(conn, attach).await
    } else {
        attach_leaf(conn, attach).await
    };

    match result {
        Ok(()) => Ok(true),
        Err(err)
            if matches!(
                err.as_database_error().and_then(|error| error.code()).as_deref(),
                Some(LOCK_NOT_AVAILABLE | DEADLOCK_DETECTED)
            ) =>
        {
            Ok(false)
        }
        Err(err) => Err(err).with_context(|| {
            format!("attaching index for transaction event day partition {}", attach.leaf)
        }),
    }
}

/// Whether attaching one more valid leaf index makes `class_index` valid.
///
/// The leaf being attached is the only one not yet counted, so this holds when
/// every other partition of the class already has a valid attached index.
async fn completes_class_index(conn: &mut sqlx::PgConnection, class_index: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT NOT i.indisvalid AND c.relispartition \
           AND (SELECT count(*) FROM pg_inherits p \
                JOIN pg_index leaf ON leaf.indexrelid = p.inhrelid \
                WHERE p.inhparent = i.indexrelid AND leaf.indisvalid) \
             = (SELECT count(*) FROM pg_inherits p WHERE p.inhparent = i.indrelid) - 1 \
         FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE i.indexrelid = to_regclass($1)",
    )
    .bind(format!("public.{class_index}"))
    .fetch_one(&mut *conn)
    .await?)
}

async fn attach_leaf(
    conn: &mut sqlx::PgConnection,
    attach: &PendingAttach,
) -> Result<(), sqlx::Error> {
    sqlx::query(&format!("SET lock_timeout = {}", ATTACH_LOCK_TIMEOUT.as_millis()))
        .execute(&mut *conn)
        .await?;
    let result = sqlx::query(&attach.statement()).execute(&mut *conn).await;
    sqlx::query("SET lock_timeout = 0").execute(&mut *conn).await?;
    result.map(drop)
}

/// Attaches a leaf whose ATTACH also validates the root index.
///
/// Validating the root takes ACCESS EXCLUSIVE on the root index and table
/// while the leaf index is already locked. Reads lock the root before any
/// leaf, so a read already running on the root that then reaches this leaf
/// would deadlock with the ATTACH. Locking the root table first, in the same
/// order as reads, avoids that.
async fn attach_under_root_lock(
    conn: &mut sqlx::PgConnection,
    attach: &PendingAttach,
) -> Result<(), sqlx::Error> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let result = async {
        sqlx::query(&format!("SET LOCAL lock_timeout = {}", ROOT_LOCK_TIMEOUT.as_millis()))
            .execute(&mut *tx)
            .await?;
        sqlx::query("LOCK TABLE ONLY public.transaction_events IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await?;
        sqlx::query(&attach.statement()).execute(&mut *tx).await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => tx.commit().await,
        Err(err) => {
            tx.rollback().await?;
            Err(err)
        }
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
        // The first leaf in build order that is followed by another leaf of its
        // class, so its first attempt uses ATTACH_LOCK_TIMEOUT and every later
        // leaf is attached after that times out.
        let class_of = |leaf: &str| leaf.rsplit_once('_').map(|(class, _)| class.to_owned());
        let blocked = leaves
            .windows(2)
            .find(|pair| class_of(&pair[0]) == class_of(&pair[1]))
            .map(|pair| &pair[0])
            .context("migrations create a class with several day partitions")?;

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

    #[tokio::test]
    async fn completing_attach_does_not_deadlock_with_reads() -> Result<()> {
        let container = Postgres::default().with_tag("17-alpine").start().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let database_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        PgTransactionEventSink::migrate(&database_url).await?;
        let pool = PgPoolOptions::new().max_connections(4).connect(&database_url).await?;

        let cold_leaves: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_inherits i \
             JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'public.transaction_events_cold'::regclass ORDER BY 1",
        )
        .fetch_all(&pool)
        .await?;
        let hot_leaf: String = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_inherits i \
             JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'public.transaction_events_hot'::regclass ORDER BY 1 LIMIT 1",
        )
        .fetch_one(&pool)
        .await?;
        // Attaching this leaf makes the cold class index valid, which also
        // validates the root index.
        let completing = cold_leaves.last().context("migrations create cold partitions")?;
        let day = hot_leaf.trim_start_matches("transaction_events_hot_");
        // A literal date so the planner prunes to one day and locks only it.
        let hot_day = format!("{}-{}-{}", &day[..4], &day[4..6], &day[6..]);

        // A read in progress on the root table, as the API's reads are: it
        // holds ACCESS SHARE on transaction_events and the day it pruned to.
        let mut reader = pool.acquire().await?;
        reader.execute("BEGIN").await?;
        sqlx::query(&format!(
            "SELECT count(*) FROM public.transaction_events \
             WHERE retention_class = 'hot' AND event_date = DATE '{hot_day}'"
        ))
        .execute(&mut *reader)
        .await?;

        let index = tokio::spawn({
            let database_url = database_url.clone();
            async move { index_transaction_event_partitions(&database_url).await }
        });

        // Wait until the command is blocked on the root table or root index.
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE NOT granted AND relation IN \
                 ('public.transaction_events'::regclass, \
                  'public.transaction_events_ingested_at_idx'::regclass))",
            )
            .fetch_one(&pool)
            .await?;
            if waiting {
                break;
            }
            ensure!(!index.is_finished(), "index command finished before reaching the root lock");
            ensure!(Instant::now() < deadline, "index command never waited on the root");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let holds_leaf: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_locks \
             WHERE relation = to_regclass($1) AND mode = 'AccessExclusiveLock' AND granted)",
        )
        .bind(format!("public.{completing}_ingested_at_idx"))
        .fetch_one(&pool)
        .await?;
        assert!(!holds_leaf, "the command waits for the root without holding the leaf index");

        // The same read now touches the completing leaf. If the command held
        // that leaf's index while waiting for the root, this would deadlock.
        let day = completing.trim_start_matches("transaction_events_cold_");
        let cold_day = format!("{}-{}-{}", &day[..4], &day[4..6], &day[6..]);
        sqlx::query(&format!(
            "SELECT count(*) FROM public.transaction_events \
             WHERE retention_class = 'cold' AND event_date = DATE '{cold_day}'"
        ))
        .execute(&mut *reader)
        .await
        .context("read during the completing attach")?;
        reader.execute("COMMIT").await?;

        index.await??;
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
