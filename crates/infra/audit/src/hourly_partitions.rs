//! Mixed daily/hourly maintenance and explicitly gated owner-only activation.

use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use sqlx::{Connection, PgConnection, migrate::Migrate};
use tracing::warn;

use crate::{
    MAX_TRANSACTION_EVENT_FUTURE_SKEW_SECS, Metrics, TRANSACTION_EVENT_PARTITION_DAYS_AHEAD,
    TransactionEventRetentionConfig, TransactionEventRetentionOutcome,
};

/// Future HOT/WARM days contain UTC event-hour leaves; COLD remains daily.
#[derive(Debug, Clone, Copy)]
pub struct HourlyTransactionEventPartitions;

impl HourlyTransactionEventPartitions {
    /// Reads policy without requiring expansion on a legacy v1/v2 database.
    pub async fn cutoff(conn: &mut PgConnection) -> Result<Option<NaiveDate>> {
        let expanded: bool = sqlx::query_scalar(
            "SELECT to_regclass('public.transaction_events_partition_policy') IS NOT NULL",
        )
        .fetch_one(&mut *conn)
        .await?;
        if !expanded {
            return Ok(None);
        }
        Ok(sqlx::query_scalar(
            "SELECT hourly_from FROM public.transaction_events_partition_policy WHERE singleton",
        )
        .fetch_one(conn)
        .await?)
    }

    /// Parses only canonical HOT/WARM hour names, including a valid UTC date/hour.
    pub fn parse(name: &str) -> Result<(&'static str, DateTime<Utc>)> {
        let suffix = name.strip_prefix("transaction_events_").unwrap_or_default();
        let (class, hour) = suffix.split_once('_').unwrap_or_default();
        ensure!(
            matches!(class, "hot" | "warm")
                && hour.len() == 10
                && hour.bytes().all(|b| b.is_ascii_digit()),
            "unexpected hour relation: {name}"
        );
        let time = NaiveDateTime::parse_from_str(&format!("{hour}0000"), "%Y%m%d%H%M%S")?.and_utc();
        Ok((if class == "hot" { "hot" } else { "warm" }, time))
    }

    /// Children of a detached DAY remain that day's responsibility until its
    /// drop succeeds; do not misclassify them as independently detached hours.
    /// Catalog coverage includes only hours attached under the expected day/class/root.
    pub async fn hours(conn: &mut PgConnection) -> Result<Vec<(String, bool)>> {
        Ok(sqlx::query_as(
            "WITH tree AS MATERIALIZED (SELECT * FROM pg_partition_tree('public.transaction_events'::regclass)) \
             SELECT c.relname::text, CASE WHEN t.relid IS NULL THEN false ELSE public.transaction_events_validate_hour(split_part(c.relname,'_',3),(to_date(left(right(c.relname,10),8),'YYYYMMDD')::timestamp AT TIME ZONE 'UTC')+make_interval(hours=>right(c.relname,2)::int))=c.oid END \
             FROM pg_class c LEFT JOIN tree t ON t.relid=c.oid \
             WHERE c.relnamespace='public'::regnamespace AND c.relkind='r' AND c.relname ~ '^transaction_events_(hot|warm)_[0-9]{10}$' \
               AND (t.relid IS NOT NULL OR NOT EXISTS (SELECT 1 FROM pg_inherits i WHERE i.inhrelid=c.oid)) ORDER BY c.relname",
        ).fetch_all(conn).await?)
    }

    /// Runs hour DDL in independent transactions; detach releases locks before unlink.
    pub async fn ddl(
        conn: &mut PgConnection,
        class: &str,
        hour: DateTime<Utc>,
        action: &'static str,
        config: TransactionEventRetentionConfig,
        outcome: &mut TransactionEventRetentionOutcome,
    ) -> Result<bool> {
        ensure!(
            matches!(action, "create" | "detach" | "drop_detached"),
            "unexpected hour DDL action"
        );
        let mut tx = conn.begin().await?;
        sqlx::query(&format!("SET LOCAL lock_timeout = '{}ms'", config.partition_lock_timeout_ms))
            .execute(&mut *tx)
            .await?;
        let result = sqlx::query_scalar::<_, bool>(&format!(
            "SELECT public.transaction_events_{action}_hour($1,$2)"
        ))
        .bind(class)
        .bind(hour)
        .fetch_one(&mut *tx)
        .await;
        match result {
            Ok(changed) => {
                tx.commit().await?;
                Ok(changed)
            }
            Err(error) => {
                tx.rollback().await?;
                if error.as_database_error().and_then(|e| e.code()).as_deref() == Some("55P03") {
                    outcome.lock_timeouts += 1;
                    Metrics::transaction_event_partition_lock_timeouts(
                        if action == "drop_detached" { "drop" } else { action },
                    )
                    .increment(1);
                    warn!(%error, class, %hour, action, "hour partition DDL timed out; retry next pass");
                    return Ok(false);
                }
                Err(error.into())
            }
        }
    }

    /// Called under the existing retention advisory lock after daily maintenance.
    /// Reconciles missing hours (including partial-day gaps) without recreating expiry.
    pub async fn maintain(
        conn: &mut PgConnection,
        now: DateTime<Utc>,
        config: TransactionEventRetentionConfig,
        outcome: &mut TransactionEventRetentionOutcome,
    ) -> Result<()> {
        let Some(cutoff) = Self::cutoff(conn).await? else {
            return Ok(());
        };
        let existing = Self::hours(conn).await?;
        let mut attached = BTreeSet::new();
        for (name, is_attached) in existing {
            let (class, hour) = Self::parse(&name)?;
            let days = if class == "hot" { config.hot_days } else { config.warm_days };
            let expired = hour + Duration::hours(1)
                <= now - Duration::days(i64::from(days)) - Duration::hours(1);
            if !is_attached
                || (expired && Self::ddl(conn, class, hour, "detach", config, outcome).await?)
            {
                if Self::ddl(conn, class, hour, "drop_detached", config, outcome).await? {
                    outcome.partitions_dropped += 1;
                    Metrics::transaction_event_partitions_dropped(class).increment(1);
                }
            } else if !expired {
                attached.insert((class.to_owned(), hour));
            }
        }
        let end_day = (now + Duration::seconds(MAX_TRANSACTION_EVENT_FUTURE_SKEW_SECS))
            .date_naive()
            + Duration::days(i64::from(TRANSACTION_EVENT_PARTITION_DAYS_AHEAD) + 1);
        let end = end_day.and_hms_opt(0, 0, 0).unwrap().and_utc();
        for (class, days) in [("hot", config.hot_days), ("warm", config.warm_days)] {
            // Keep the complete grace interval, so retries/retention do not fight.
            let start = now - Duration::days(i64::from(days)) - Duration::hours(1);
            let mut hour = start
                .with_minute(0)
                .unwrap()
                .with_second(0)
                .unwrap()
                .with_nanosecond(0)
                .unwrap()
                .max(cutoff.and_hms_opt(0, 0, 0).unwrap().and_utc());
            while hour < end {
                if !attached.contains(&(class.to_owned(), hour))
                    && Self::ddl(conn, class, hour, "create", config, outcome).await?
                {
                    outcome.partitions_created += 1;
                    Metrics::transaction_event_partitions_created(class).increment(1);
                }
                hour += Duration::hours(1);
            }
        }
        Ok(())
    }

    /// Checks every attached day before maintenance or coverage trusts its name.
    /// Legacy databases without the expand migration keep their existing path.
    pub async fn validate_days(conn: &mut PgConnection) -> Result<()> {
        let expanded: bool = sqlx::query_scalar(
            "SELECT to_regclass('public.transaction_events_partition_policy') IS NOT NULL",
        )
        .fetch_one(&mut *conn)
        .await?;
        if expanded {
            sqlx::query("SELECT public.transaction_events_validate_class(c) FROM unnest(ARRAY['hot','warm','cold']) c")
                .execute(&mut *conn)
                .await?;
            let classes: bool = sqlx::query_scalar("SELECT count(*)=3 AND bool_and(t.relid IN ('public.transaction_events_hot'::regclass,'public.transaction_events_warm'::regclass,'public.transaction_events_cold'::regclass)) AND NOT EXISTS(SELECT 1 FROM pg_index WHERE indrelid IN (SELECT relid FROM pg_partition_tree('public.transaction_events'::regclass)) AND indisunique AND NOT indisprimary) FROM pg_partition_tree('public.transaction_events'::regclass) t WHERE t.level=1")
                .fetch_one(&mut *conn).await?;
            ensure!(classes, "unexpected retention-class subtree");
            let days: bool = sqlx::query_scalar("SELECT COALESCE(bool_and(c.relname ~ '^transaction_events_(hot|warm|cold)_[0-9]{8}$' AND public.transaction_events_validate_day(split_part(c.relname,'_',3),to_date(right(c.relname,8),'YYYYMMDD'))=c.oid),true) FROM pg_partition_tree('public.transaction_events'::regclass) t JOIN pg_class c ON c.oid=t.relid WHERE t.level=2")
                .fetch_one(conn)
                .await?;
            ensure!(days, "unexpected day name/OID ancestry");
        }
        Ok(())
    }

    /// Uses actual hour coverage, not an empty day branch, for horizon reporting.
    pub async fn horizons(
        conn: &mut PgConnection,
        now: DateTime<Utc>,
    ) -> Result<Vec<(&'static str, f64)>> {
        Self::validate_days(conn).await?;
        let Some(cutoff) = Self::cutoff(conn).await? else {
            return Ok(Vec::new());
        };
        let hours = Self::hours(conn).await?;
        let mut coverage = BTreeSet::new();
        for (name, attached) in hours {
            if attached {
                let (class, hour) = Self::parse(&name)?;
                coverage.insert((class.to_owned(), hour));
            }
        }
        let days: Vec<(String,NaiveDate)>=sqlx::query_as(
            "SELECT split_part(c.relname,'_',3),to_date(right(c.relname,8),'YYYYMMDD') FROM pg_partition_tree('public.transaction_events'::regclass) t JOIN pg_class c ON c.oid=t.relid WHERE c.relkind='r' AND c.relname ~ '^transaction_events_(hot|warm)_[0-9]{8}$'",
        ).fetch_all(conn).await?;
        let mut result = Vec::new();
        for class in ["hot", "warm"] {
            let mut hour =
                now.with_minute(0).unwrap().with_second(0).unwrap().with_nanosecond(0).unwrap();
            loop {
                let covered = if hour.date_naive() < cutoff {
                    days.contains(&(class.to_owned(), hour.date_naive()))
                } else {
                    coverage.contains(&(class.to_owned(), hour))
                };
                if !covered {
                    break;
                }
                hour += Duration::hours(1);
            }
            result.push((
                class,
                f64::from(
                    i32::try_from(
                        (hour - now)
                            .num_seconds()
                            .saturating_sub(MAX_TRANSACTION_EVENT_FUTURE_SKEW_SECS)
                            .max(0),
                    )
                    .unwrap_or(i32::MAX),
                ),
            ));
        }
        Ok(result)
    }

    /// Read-only validation of active bucket keys, local uniqueness and bounds.
    /// Names alone are not proof of hourly coverage or the accepted dedupe contract.
    pub async fn validate(conn: &mut PgConnection) -> Result<()> {
        Self::validate_days(conn).await?;
        let Some(cutoff) = Self::cutoff(conn).await? else {
            return Ok(());
        };
        // The owner SQL helpers also guard retention DDL. Check the entire
        // expected ancestor chain, not each canonical name/bound in isolation.
        sqlx::query("SELECT public.transaction_events_validate_class(c) FROM unnest(ARRAY['hot','warm','cold']) c")
            .execute(&mut *conn)
            .await?;
        sqlx::query("SELECT public.transaction_events_validate_hour(split_part(c.relname,'_',3),(to_date(left(right(c.relname,10),8),'YYYYMMDD')::timestamp AT TIME ZONE 'UTC')+make_interval(hours=>right(c.relname,2)::int)) FROM pg_partition_tree('public.transaction_events'::regclass) t JOIN pg_class c ON c.oid=t.relid WHERE t.level=3")
            .execute(&mut *conn)
            .await?;
        let root_keys: bool = sqlx::query_scalar(
            "SELECT count(*)=4 FROM pg_partitioned_table p JOIN pg_class c ON c.oid=p.partrelid JOIN pg_attribute a ON a.attrelid=c.oid AND a.attname=CASE WHEN c.oid='public.transaction_events'::regclass THEN 'retention_class' ELSE 'event_date' END WHERE c.oid IN ('public.transaction_events'::regclass,'public.transaction_events_hot'::regclass,'public.transaction_events_warm'::regclass,'public.transaction_events_cold'::regclass) AND p.partnatts=1 AND p.partattrs[0]=a.attnum AND p.partexprs IS NULL AND ((c.oid='public.transaction_events'::regclass AND p.partstrat='l') OR (c.oid<>'public.transaction_events'::regclass AND p.partstrat='r' AND pg_get_expr(c.relpartbound,c.oid)=format('FOR VALUES IN (%L)',split_part(c.relname,'_',3))))"
        ).fetch_one(&mut *conn).await?;
        ensure!(root_keys, "mixed partition root/class keys invalid");
        let day_bounds: Vec<(String,NaiveDate,NaiveDate)> = sqlx::query_as(
            r"SELECT c.relname::text, (regexp_match(pg_get_expr(c.relpartbound,c.oid), '^FOR VALUES FROM \(''([^'']+)''\) TO \(''([^'']+)''\)$'))[1]::date, (regexp_match(pg_get_expr(c.relpartbound,c.oid), '^FOR VALUES FROM \(''([^'']+)''\) TO \(''([^'']+)''\)$'))[2]::date FROM pg_partition_tree('public.transaction_events'::regclass) t JOIN pg_class c ON c.oid=t.relid WHERE t.level=2"
        ).fetch_all(&mut *conn).await?;
        for (name, start, end) in day_bounds {
            let suffix = name.strip_prefix("transaction_events_").unwrap_or_default();
            let (class, date) = suffix.split_once('_').unwrap_or_default();
            ensure!(
                matches!(class, "hot" | "warm" | "cold") && date.len() == 8,
                "unexpected day relation: {name}"
            );
            let day = NaiveDate::parse_from_str(date, "%Y%m%d")?;
            ensure!(
                (start, end) == (day, day + Duration::days(1)),
                "day bounds/name mismatch: {name}"
            );
        }
        let parents: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS (SELECT 1 FROM pg_index WHERE indisprimary AND indrelid IN ('public.transaction_events'::regclass,'public.transaction_events_hot'::regclass,'public.transaction_events_warm'::regclass)) AND EXISTS (SELECT 1 FROM pg_index WHERE indisprimary AND indisvalid AND indrelid='public.transaction_events_cold'::regclass)"
        ).fetch_one(&mut *conn).await?;
        ensure!(parents, "hourly parent uniqueness contract invalid");
        let daily_valid: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS (SELECT 1 FROM pg_partition_tree('public.transaction_events'::regclass) t JOIN pg_class c ON c.oid=t.relid WHERE t.isleaf AND t.level=2 AND (c.relname !~ '^transaction_events_(hot|warm|cold)_[0-9]{8}$' OR (split_part(c.relname,'_',3) IN ('hot','warm') AND to_date(right(c.relname,8),'YYYYMMDD') >= $1) OR NOT EXISTS (SELECT 1 FROM pg_index i JOIN pg_attribute id ON id.attrelid=c.oid AND id.attname='event_id' JOIN pg_attribute cl ON cl.attrelid=c.oid AND cl.attname='retention_class' JOIN pg_attribute dt ON dt.attrelid=c.oid AND dt.attname='event_date' WHERE i.indrelid=c.oid AND i.indisprimary AND i.indisvalid AND i.indisready AND i.indnkeyatts=3 AND i.indnatts=3 AND i.indkey[0]=id.attnum AND i.indkey[1]=cl.attnum AND i.indkey[2]=dt.attnum AND i.indpred IS NULL AND i.indexprs IS NULL)))"
        ).bind(cutoff).fetch_one(&mut *conn).await?;
        ensure!(daily_valid, "retained daily leaf uniqueness/cutoff contract invalid");
        let branches: Vec<(String,bool)> = sqlx::query_as(
            "SELECT c.relname::text, p.partstrat='r' AND p.partnatts=1 AND p.partattrs[0]=a.attnum AND p.partexprs IS NULL FROM pg_partition_tree('public.transaction_events'::regclass) t JOIN pg_class c ON c.oid=t.relid JOIN pg_partitioned_table p ON p.partrelid=c.oid LEFT JOIN pg_attribute a ON a.attrelid=c.oid AND a.attname='event_time' WHERE t.level=2"
        ).fetch_all(&mut *conn).await?;
        for (name, correct_key) in branches {
            ensure!(
                correct_key
                    && (name.starts_with("transaction_events_hot_")
                        || name.starts_with("transaction_events_warm_")),
                "unexpected nested partition key: {name}"
            );
            let day = NaiveDate::parse_from_str(
                name.rsplit_once('_').map(|(_, day)| day).unwrap_or_default(),
                "%Y%m%d",
            )?;
            ensure!(day >= cutoff, "hourly branch precedes cutoff: {name}");
        }
        // Bounds and PK ownership are read in the same catalog statement,
        // independent of session timezone and without one round trip per hour.
        let leaves: Vec<(String,bool,DateTime<Utc>,DateTime<Utc>)> = sqlx::query_as(
            r"SELECT c.relname::text, EXISTS(SELECT 1 FROM pg_index i JOIN pg_attribute a ON a.attrelid=c.oid AND a.attname='event_id' WHERE i.indrelid=c.oid AND i.indisprimary AND i.indisvalid AND i.indisready AND i.indnkeyatts=1 AND i.indnatts=1 AND i.indkey[0]=a.attnum AND i.indpred IS NULL AND i.indexprs IS NULL), (regexp_match(pg_get_expr(c.relpartbound,c.oid), '^FOR VALUES FROM \(''([^'']+)''\) TO \(''([^'']+)''\)$'))[1]::timestamptz, (regexp_match(pg_get_expr(c.relpartbound,c.oid), '^FOR VALUES FROM \(''([^'']+)''\) TO \(''([^'']+)''\)$'))[2]::timestamptz FROM pg_partition_tree('public.transaction_events'::regclass) t JOIN pg_class c ON c.oid=t.relid WHERE t.level=3"
        ).fetch_all(&mut *conn).await?;
        for (name, primary, start, end) in leaves {
            let (_, hour) = Self::parse(&name)?;
            ensure!(
                primary && hour.date_naive() >= cutoff,
                "hour PK/cutoff contract invalid: {name}"
            );
            ensure!(
                (start, end) == (hour, hour + Duration::hours(1)),
                "hour bounds/name mismatch: {name}"
            );
        }
        Ok(())
    }

    /// Checks canonical BRIN definitions/edges without requiring historical backfill.
    /// `complete` is only for the explicit manual index command's final result.
    pub async fn validate_indexes(conn: &mut PgConnection, complete: bool) -> Result<()> {
        sqlx::query("SELECT public.transaction_events_validate_brin(t.relid::oid,CASE WHEN t.parentrelid IS NULL THEN NULL WHEN $1 OR NOT t.isleaf OR EXISTS(SELECT 1 FROM pg_inherits WHERE inhrelid=to_regclass(format('public.%I',c.relname||'_ingested_at_idx'))) THEN to_regclass(format('public.%I',p.relname||'_ingested_at_idx'))::oid ELSE NULL END,$1) FROM pg_partition_tree('public.transaction_events'::regclass) t JOIN pg_class c ON c.oid=t.relid LEFT JOIN pg_class p ON p.oid=t.parentrelid WHERE $1 OR NOT t.isleaf OR to_regclass(format('public.%I',c.relname||'_ingested_at_idx')) IS NOT NULL")
            .bind(complete)
            .execute(conn)
            .await?;
        Ok(())
    }

    /// Owner-only opt-in activation. No production T is selected by normal migration.
    /// Preparation checks and ALL writer upgrades must precede this call. It acquires
    /// the real sqlx migration lock before the SQL function's retention lock.
    pub async fn activate(
        database_url: &str,
        day: NaiveDate,
        all_writers_bridged: bool,
    ) -> Result<()> {
        let mut conn = PgConnection::connect(database_url).await?;
        let result = async {
            sqlx::query("SET lock_timeout='5s'").execute(&mut conn).await?;
            sqlx::query("SET statement_timeout='30s'").execute(&mut conn).await?;
            conn.lock().await?;
            let mut tx = conn.begin().await?;
            sqlx::query("SET LOCAL lock_timeout='5s'").execute(&mut *tx).await?;
            sqlx::query("SET LOCAL statement_timeout='30s'").execute(&mut *tx).await?;
            sqlx::query("SELECT public.transaction_events_activate_hourly($1,$2)")
                .bind(day)
                .bind(all_writers_bridged)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            anyhow::Ok(())
        }
        .await;
        let unlock = conn.unlock().await;
        let close = conn.close().await;
        result?;
        unlock?;
        close?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::HourlyTransactionEventPartitions;

    #[test]
    fn canonical_hours_only() {
        assert!(
            HourlyTransactionEventPartitions::parse("transaction_events_hot_2026100201").is_ok()
        );
        for name in [
            "transaction_events_cold_2026100201",
            "transaction_events_hot_2026100224",
            "transaction_events_hot_2026023001",
            "transaction_events_hot_20261002",
            "transaction_events_hot_2026100201;drop",
        ] {
            assert!(HourlyTransactionEventPartitions::parse(name).is_err(), "{name}");
        }
    }
}
