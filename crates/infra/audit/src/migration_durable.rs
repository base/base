//! Purpose-specific durable retry generations, independent of pod-local cache.

use sqlx::{Executor, PgConnection};
use tokio::time::{Duration, sleep};

use crate::{AuditMigration, MigrationError, MigrationRecord, MigrationReporter, MigrationSession};

/// Actual target and reviewed retry generation. Never a pod identity.
#[derive(Clone)]
pub struct MigrationDurableKey {
    /// Native target token bound to the observed database OID and writer endpoint.
    pub target: String,
    /// Stable deployment-configured retry generation.
    pub generation: String,
}

impl std::fmt::Debug for MigrationDurableKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MigrationDurableKey { target redacted }")
    }
}

/// Native durable store; no generic job framework or deployment-side SQL.
#[derive(Debug, Clone, Copy)]
pub struct MigrationDurable;

impl MigrationDurable {
    /// Distinct from sqlx's database migration lock; serializes record decisions.
    pub const GATE: i64 = 0x4155_4449_5452_554e;

    /// Acquires the generation gate with bounded queries and a responsive stop gate.
    pub async fn lock(
        conn: &mut PgConnection,
        progress: &MigrationReporter,
    ) -> Result<(), MigrationError> {
        progress.phase(crate::MigrationPhase::WaitingForLock).await?;
        loop {
            progress.check_running()?;
            let acquired: bool = progress
                .io(async {
                    sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                        .bind(Self::GATE)
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(|_| MigrationError::StateIo)
                })
                .await?;
            if acquired {
                return Ok(());
            }
            tokio::select! {
                _ = progress.cancel.cancelled() => return Err(MigrationError::StopRequested),
                _ = sleep(Duration::from_millis(100)) => {}
            }
        }
    }

    /// Releases only this control session's purpose-specific gate.
    pub async fn unlock(conn: &mut PgConnection) -> Result<(), MigrationError> {
        let released: bool = tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_scalar("SELECT pg_advisory_unlock($1)").bind(Self::GATE).fetch_one(conn),
        )
        .await
        .map_err(|_| MigrationError::StateIo)?
        .map_err(|_| MigrationError::StateIo)?;
        if released { Ok(()) } else { Err(MigrationError::StateIo) }
    }

    /// Bootstraps the NEW immutable migration SQL without recording schema history.
    /// A denied bootstrap fails before the schema/index worker is dispatched.
    pub async fn bootstrap(
        conn: &mut PgConnection,
        generation: &str,
    ) -> Result<MigrationDurableKey, MigrationError> {
        conn.execute(include_str!("../migrations/003_native_migration_runs.sql"))
            .await
            .map_err(|_| MigrationError::StateIo)?;
        sqlx::query("INSERT INTO public.audit_migration_identity(singleton,target_token) VALUES(true,$1) ON CONFLICT(singleton) DO NOTHING")
            .bind(MigrationSession::nonce()).execute(&mut *conn).await.map_err(|_| MigrationError::StateIo)?;
        let target: String = sqlx::query_scalar("SELECT target_token || ':' || (SELECT oid::text FROM pg_database WHERE datname=current_database()) || ':' || coalesce(inet_server_addr()::text,'local') || ':' || coalesce(inet_server_port()::text,'local') FROM public.audit_migration_identity WHERE singleton")
            .fetch_one(conn).await.map_err(|_| MigrationError::StateIo)?;
        Ok(MigrationDurableKey { target, generation: generation.into() })
    }

    /// Restores only the exact current target, generation, and validator contract.
    pub async fn load(
        conn: &mut PgConnection,
        key: &MigrationDurableKey,
    ) -> Result<Option<MigrationRecord>, MigrationError> {
        let row: Option<(String, String, serde_json::Value)> = sqlx::query_as("SELECT target_id,fingerprint,record FROM public.audit_migration_runs WHERE generation=$1")
            .bind(&key.generation).fetch_optional(conn).await.map_err(|_| MigrationError::StateIo)?;
        let Some((target, fingerprint, value)) = row else {
            return Ok(None);
        };
        if target != key.target || fingerprint != AuditMigration::fingerprint() {
            return Err(MigrationError::StateCorrupt);
        }
        let record: MigrationRecord =
            serde_json::from_value(value).map_err(|_| MigrationError::StateCorrupt)?;
        if record.target.as_deref() != Some(key.target.as_str())
            || record.fingerprint != fingerprint
            || record.status.run_id != key.generation
            || record.status.version != 1
        {
            return Err(MigrationError::StateCorrupt);
        }
        record.validate()?;
        Ok(Some(record))
    }

    /// Returns unconfirmed owners; a previously proven cleanup needs no new-server absence claim.
    pub async fn owners(
        conn: &mut PgConnection,
        key: &MigrationDurableKey,
    ) -> Result<Vec<crate::MigrationBackend>, MigrationError> {
        let values: Vec<serde_json::Value> =
            sqlx::query_scalar("SELECT record FROM public.audit_migration_runs")
                .fetch_all(conn)
                .await
                .map_err(|_| MigrationError::StateIo)?;
        let mut owners = Vec::new();
        for value in values {
            let record: MigrationRecord =
                serde_json::from_value(value).map_err(|_| MigrationError::StateCorrupt)?;
            record.validate()?;
            if record.status.cleanup_confirmed
                && record.backend.as_ref().is_some_and(|owner| owner.server.is_some())
            {
                continue;
            }
            if record.target.as_deref() != Some(key.target.as_str()) {
                // A changed endpoint/generation cannot hide a prior unconfirmed owner.
                // Nor can it authorize signalling an owner on a foreign server.
                return Err(MigrationError::StateCorrupt);
            }
            if let Some(owner) = record.backend {
                owners.push(owner);
            }
        }
        Ok(owners)
    }

    /// Persists verified absence for every matching prior record without deleting its owner.
    /// Server/login provenance and absence are checked atomically with the marker update.
    pub async fn confirm_owner(
        conn: &mut PgConnection,
        key: &MigrationDurableKey,
        owner: &crate::MigrationBackend,
    ) -> Result<(), MigrationError> {
        let server = owner.server.as_ref().ok_or(MigrationError::CancellationUnconfirmed)?;
        let value = serde_json::to_value(owner).map_err(|_| MigrationError::StateIo)?;
        let statement = format!(
            "UPDATE public.audit_migration_runs SET record=jsonb_set(record,'{{status,cleanup_confirmed}}','true'::jsonb),updated_at=now() WHERE target_id=$10 AND record->'backend'=$11 AND {} AND NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$7 AND backend_start=$8 AND datname=$2 AND usename=$1 AND application_name=$9)",
            MigrationSession::OBSERVER
        );
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query(&statement)
                .bind(&owner.role)
                .bind(&owner.database)
                .bind(server.started_at)
                .bind(&server.address)
                .bind(server.port)
                .bind(server.database_oid)
                .bind(owner.pid)
                .bind(owner.started_at)
                .bind(&owner.application)
                .bind(&key.target)
                .bind(value)
                .execute(conn),
        )
        .await
        .map_err(|_| MigrationError::StateIo)?
        .map_err(|_| MigrationError::StateIo)?;
        if result.rows_affected() > 0 {
            Ok(())
        } else {
            Err(MigrationError::CancellationUnconfirmed)
        }
    }

    /// Commits ownership before DDL or terminal state before completion is announced.
    pub async fn save(
        conn: &mut PgConnection,
        key: &MigrationDurableKey,
        progress: &MigrationReporter,
    ) -> Result<(), MigrationError> {
        let record = MigrationRecord {
            fingerprint: AuditMigration::fingerprint(),
            target: Some(key.target.clone()),
            status: progress.snapshot(),
            backend: progress.backend.lock().map_err(|_| MigrationError::Worker)?.clone(),
        };
        Self::save_record(conn, key, record).await
    }

    /// Writes a candidate record before it becomes visible as terminal over HTTP.
    pub async fn save_record(
        conn: &mut PgConnection,
        key: &MigrationDurableKey,
        record: MigrationRecord,
    ) -> Result<(), MigrationError> {
        let value = serde_json::to_value(record).map_err(|_| MigrationError::StateIo)?;
        tokio::time::timeout(Duration::from_secs(2), sqlx::query("INSERT INTO public.audit_migration_runs(generation,target_id,fingerprint,record) VALUES($1,$2,$3,$4) ON CONFLICT(generation) DO UPDATE SET record=EXCLUDED.record,updated_at=now() WHERE audit_migration_runs.target_id=EXCLUDED.target_id AND audit_migration_runs.fingerprint=EXCLUDED.fingerprint")
            .bind(&key.generation).bind(&key.target).bind(AuditMigration::fingerprint()).bind(value)
            .execute(conn)).await.map_err(|_| MigrationError::StateIo)?.map_err(|_| MigrationError::StateIo).and_then(|result| if result.rows_affected()==1 { Ok(()) } else { Err(MigrationError::StateCorrupt) })
    }
}
