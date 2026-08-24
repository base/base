//! Implements the public `base` RPC namespace: read-only, non-admin endpoints for node operators.

use core::str::FromStr;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use base_consensus_gossip::Metrics;
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, PackedProtocolVersion, UpgradeReadiness, UpgradeSignalConfig,
    UpgradeSignalDefaults, UpgradeSignalError, UpgradeSignalMetricLayer, UpgradeSignalSchedule,
};
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObject},
};
use tracing::warn;

use crate::BaseApiServer;

/// A cached outcome of an L1 schedule read, used to collapse bursts of readiness queries into a
/// single read.
///
/// Both successes and failures are cached (failures for a shorter window, see
/// [`BaseRpc::ERROR_CACHE_TTL`]): during an L1 outage a burst of queries would otherwise turn into
/// one full retrying read per caller, since the refresh lock only serializes them. Caching the
/// failure lets waiters fail fast off the first caller's result and recover quickly once L1 is
/// healthy again.
#[derive(Debug)]
struct CachedRead {
    /// When the read completed.
    read_at: Instant,
    /// The outcome of the read: `Ok(None)` = the contract had no schedule (empty), `Ok(Some)` = the
    /// schedule read from L1, `Err` = the read failed (message retained for server-side logging).
    outcome: Result<Option<UpgradeSignalSchedule>, String>,
}

/// Server implementation of the public [`crate::BaseApiServer`] (`base` namespace).
///
/// Holds the node's upgrade-signal configuration and a contract reader so it can answer readiness
/// queries with a fresh, authoritative L1 read (which also confirms the node can reach the contract).
/// It is read-only and never mutates node state, so it is safe to expose on the public RPC.
#[derive(Debug)]
pub struct BaseRpc {
    /// Upgrade-signal schedule read configuration (carries this node's advertised protocol version).
    pub upgrade_signal_config: UpgradeSignalConfig,
    /// Hardened L1 contract reader built from `upgrade_signal_config`.
    pub upgrade_signal_reader: AlloyUpgradeSignalReader,
    /// Short-TTL cache of the last L1 schedule read outcome, shared across concurrent RPC requests.
    schedule_cache: Mutex<Option<CachedRead>>,
    /// Serializes L1 schedule refreshes so a burst of cache misses coalesces into one read.
    ///
    /// Unlike [`schedule_cache`](Self::schedule_cache) (only locked to read or write the cached
    /// value), this is held across the L1 read. Concurrent missing callers wait here, then re-check
    /// the cache and serve the value the first caller wrote — so the public endpoint never amplifies
    /// a burst into one L1 read per caller.
    refresh_lock: tokio::sync::Mutex<()>,
}

impl BaseRpc {
    /// How long a schedule read is reused before the next query reads L1 again.
    ///
    /// This endpoint is on the public, unauthenticated RPC and each miss reads the L1 contract (with
    /// the reader's own retries). A short TTL collapses a burst of queries into a single L1 read,
    /// bounding the amplification into L1 traffic. It is intentionally short — a few seconds — so it
    /// only dedups bursts and does not meaningfully stale the answer: the node itself polls L1 far
    /// less often, and a schedule changes only on a (rare) governance action.
    const CACHE_TTL: Duration = Duration::from_secs(5);

    /// How long a *failed* read is cached before the next query reads L1 again.
    ///
    /// Kept shorter than [`Self::CACHE_TTL`] so a burst during an L1 outage collapses into one
    /// retrying read (rather than one per queued caller) while still recovering quickly once L1 is
    /// healthy again.
    const ERROR_CACHE_TTL: Duration = Duration::from_secs(1);

    /// Creates a new `base`-namespace RPC server from the node's upgrade-signal config and reader.
    pub const fn new(
        upgrade_signal_config: UpgradeSignalConfig,
        upgrade_signal_reader: AlloyUpgradeSignalReader,
    ) -> Self {
        Self {
            upgrade_signal_config,
            upgrade_signal_reader,
            schedule_cache: Mutex::new(None),
            refresh_lock: tokio::sync::Mutex::const_new(()),
        }
    }

    /// Parses an optional `major.minor.patch[-rc.N]` target version into a packed value.
    ///
    /// Returns an `invalid params` RPC error rather than a generic failure so an operator sees
    /// exactly what was rejected.
    fn parse_target_version(
        target_version: Option<String>,
    ) -> Result<Option<alloy_primitives::U256>, ErrorObject<'static>> {
        target_version
            .map(|version| {
                PackedProtocolVersion::from_str(&version).map(PackedProtocolVersion::into_inner)
            })
            .transpose()
            .map_err(|error| ErrorObject::owned(ErrorCode::InvalidParams.code(), error, None::<()>))
    }

    /// Whether a cached outcome recorded `elapsed` ago is still fresh.
    ///
    /// Failures expire after the shorter [`Self::ERROR_CACHE_TTL`]; successes after
    /// [`Self::CACHE_TTL`].
    const fn outcome_is_fresh(
        outcome: &Result<Option<UpgradeSignalSchedule>, String>,
        elapsed: Duration,
    ) -> bool {
        let ttl = if outcome.is_ok() { Self::CACHE_TTL } else { Self::ERROR_CACHE_TTL };
        elapsed.as_nanos() < ttl.as_nanos()
    }

    /// Returns the cached read outcome when the entry is still within its TTL.
    ///
    /// The outer `Option` distinguishes a cache hit from a miss; the inner `Result` is the cached
    /// outcome — a schedule (`Ok(None)` = empty contract) or a failed read reconstructed as a
    /// generic error. A poisoned lock degrades to a miss (a fresh read).
    fn fresh_cached_read(
        &self,
    ) -> Option<Result<Option<UpgradeSignalSchedule>, UpgradeSignalError>> {
        let guard = self.schedule_cache.lock().ok()?;
        let cached = guard.as_ref()?;
        if !Self::outcome_is_fresh(&cached.outcome, cached.read_at.elapsed()) {
            return None;
        }
        Some(match &cached.outcome {
            Ok(schedule) => Ok(schedule.clone()),
            Err(message) => Err(UpgradeSignalError::provider("cached upgrade readiness", message)),
        })
    }

    /// Reads the L1 schedule, serving a recent read from the cache when one is available.
    ///
    /// Returns `Ok(None)` for an empty contract. An empty read is *not* an authoritative
    /// "nothing scheduled" — a healthy append-only contract always carries at least the oldest
    /// upgrade — but it is cached and surfaced so the readiness evaluator can report it (unready
    /// unless a `target_version` probe was supplied). Read failures are cached briefly (see
    /// [`CachedRead`]) so a burst during an L1 outage does not turn into one retrying read per
    /// caller, then retried once the short error TTL expires.
    async fn cached_schedule(&self) -> Result<Option<UpgradeSignalSchedule>, UpgradeSignalError> {
        if let Some(cached) = self.fresh_cached_read() {
            return cached;
        }

        // Coalesce concurrent misses: hold the refresh lock across the L1 read so a burst of
        // queries collapses into a single in-flight read rather than one read per caller.
        let _refresh = self.refresh_lock.lock().await;

        // Re-check under the refresh lock: a caller that held it before us may have just populated
        // the cache, in which case we skip the L1 read entirely (including a just-cached failure).
        if let Some(cached) = self.fresh_cached_read() {
            return cached;
        }

        let outcome = match self
            .upgrade_signal_config
            .read_schedule(
                &self.upgrade_signal_reader,
                "upgrade readiness",
                &[UpgradeSignalMetricLayer::Consensus],
            )
            .await
        {
            Ok(schedule) => Ok(Some(schedule)),
            Err(UpgradeSignalError::EmptySchedule) => Ok(None),
            Err(error) => Err(error),
        };

        // Cache the outcome (success or failure) so the rest of the burst reuses it. The failure is
        // stored as its message string; waiters get a generic error reconstructed from it.
        if let Ok(mut guard) = self.schedule_cache.lock() {
            *guard = Some(CachedRead {
                read_at: Instant::now(),
                outcome: match &outcome {
                    Ok(schedule) => Ok(schedule.clone()),
                    Err(error) => Err(error.to_string()),
                },
            });
        }

        outcome
    }
}

#[async_trait]
impl BaseApiServer for BaseRpc {
    async fn upgrade_readiness(
        &self,
        target_version: Option<String>,
    ) -> RpcResult<UpgradeReadiness> {
        Metrics::rpc_calls("base_upgradeReadiness").increment(1.0);

        let target = Self::parse_target_version(target_version)?;
        let now_secs = UpgradeSignalDefaults::now_secs();

        // An empty read is surfaced (not errored) so the evaluator can still answer a
        // `target_version` probe against it — but it is not treated as an authoritative "nothing
        // scheduled": a healthy append-only contract always carries at least the oldest upgrade, so
        // without a target the evaluator reports it unready rather than vacuously ready.
        let schedule = self.cached_schedule().await.map_err(|error| {
            // The error string can carry the L1 endpoint URL and upstream response body, and this is
            // the public, unauthenticated namespace — so log the detail server-side and return a
            // generic error with no `data`, matching the redaction in `admin.rs`.
            warn!(
                target: "upgrade_signal",
                error = %error,
                "failed to read L1 upgrade schedule for readiness query"
            );
            ErrorObject::owned(-32006, "failed to read L1 upgrade schedule", None::<()>)
        })?;

        // Derive the readiness inputs from the optional schedule so both the scheduled and
        // pre-schedule (empty contract) paths flow through a single `evaluate_readiness` call.
        let (signals, l1_block_number) = schedule.as_ref().map_or((&[][..], None), |schedule| {
            (schedule.signals.as_slice(), Some(schedule.l1_block_number))
        });

        Ok(self.upgrade_signal_config.evaluate_readiness(
            signals,
            l1_block_number,
            now_secs,
            target,
        ))
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use base_upgrade_signal::UpgradeSignalDefaults;

    use super::BaseRpc;

    #[test]
    fn failed_reads_expire_sooner_than_successful_ones() {
        // A success stays fresh across the short error window but expires by the success TTL.
        let success = Ok(None);
        assert!(BaseRpc::outcome_is_fresh(&success, BaseRpc::ERROR_CACHE_TTL));
        assert!(BaseRpc::outcome_is_fresh(&success, BaseRpc::CACHE_TTL - Duration::from_millis(1)));
        assert!(!BaseRpc::outcome_is_fresh(&success, BaseRpc::CACHE_TTL));

        // A failure is only reused within the shorter error TTL, so a burst during an outage
        // collapses into one retrying read while recovery stays fast.
        let failure = Err("boom".to_string());
        assert!(BaseRpc::outcome_is_fresh(
            &failure,
            BaseRpc::ERROR_CACHE_TTL - Duration::from_millis(1)
        ));
        assert!(!BaseRpc::outcome_is_fresh(&failure, BaseRpc::ERROR_CACHE_TTL));
    }

    #[test]
    fn parses_or_rejects_target_version() {
        assert_eq!(BaseRpc::parse_target_version(None).unwrap(), None);
        assert_eq!(
            BaseRpc::parse_target_version(Some("1.2.3".to_string())).unwrap(),
            Some(UpgradeSignalDefaults::packed_protocol_version(1, 2, 3))
        );

        let error = BaseRpc::parse_target_version(Some("not-a-version".to_string())).unwrap_err();
        assert_eq!(error.code(), jsonrpsee::types::ErrorCode::InvalidParams.code());
    }
}
