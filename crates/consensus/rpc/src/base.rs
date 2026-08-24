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

/// A cached L1 schedule read, used to collapse bursts of readiness queries into one L1 read.
#[derive(Debug)]
struct CachedSchedule {
    /// When the read completed.
    read_at: Instant,
    /// The schedule read from L1; `None` means the contract had no schedule (empty).
    schedule: Option<UpgradeSignalSchedule>,
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
    /// Short-TTL cache of the last L1 schedule read, shared across concurrent RPC requests.
    schedule_cache: Mutex<Option<CachedSchedule>>,
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

    /// Returns the cached schedule when the entry is still within [`Self::CACHE_TTL`].
    ///
    /// The outer `Option` distinguishes a cache hit from a miss; the inner `Option` is the cached
    /// value (`None` = the contract was empty). A poisoned lock degrades to a miss (a fresh read).
    fn fresh_cached_schedule(&self) -> Option<Option<UpgradeSignalSchedule>> {
        let guard = self.schedule_cache.lock().ok()?;
        let cached = guard.as_ref()?;
        (cached.read_at.elapsed() < Self::CACHE_TTL).then(|| cached.schedule.clone())
    }

    /// Reads the L1 schedule, serving a recent read from the cache when one is available.
    ///
    /// Returns `Ok(None)` for an empty contract (a valid pre-schedule state). Read errors propagate
    /// and are never cached, so a transient failure is retried by the next query.
    async fn cached_schedule(&self) -> Result<Option<UpgradeSignalSchedule>, UpgradeSignalError> {
        if let Some(cached) = self.fresh_cached_schedule() {
            return Ok(cached);
        }

        // Coalesce concurrent misses: hold the refresh lock across the L1 read so a burst of
        // queries collapses into a single in-flight read rather than one read per caller.
        let _refresh = self.refresh_lock.lock().await;

        // Re-check under the refresh lock: a caller that held it before us may have just populated
        // the cache, in which case we skip the L1 read entirely.
        if let Some(cached) = self.fresh_cached_schedule() {
            return Ok(cached);
        }

        let schedule = match self
            .upgrade_signal_config
            .read_schedule(
                &self.upgrade_signal_reader,
                "upgrade readiness",
                &[UpgradeSignalMetricLayer::Consensus],
            )
            .await
        {
            Ok(schedule) => Some(schedule),
            Err(UpgradeSignalError::EmptySchedule) => None,
            Err(error) => return Err(error),
        };

        if let Ok(mut guard) = self.schedule_cache.lock() {
            *guard = Some(CachedSchedule { read_at: Instant::now(), schedule: schedule.clone() });
        }

        Ok(schedule)
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

        // An empty contract is a valid pre-schedule state (the operator is likely checking a
        // `target_version` ahead of the schedule being published on L1), so it is reported as
        // "nothing scheduled" rather than as an error.
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
    use base_upgrade_signal::UpgradeSignalDefaults;

    use super::BaseRpc;

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
