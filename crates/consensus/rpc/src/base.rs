//! Implements the public `base` RPC namespace: read-only, non-admin endpoints for node operators.

use core::str::FromStr;

use async_trait::async_trait;
use base_consensus_gossip::Metrics;
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, PackedProtocolVersion, UpgradeReadiness, UpgradeSignalConfig,
    UpgradeSignalDefaults, UpgradeSignalError, UpgradeSignalMetricLayer,
};
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObject},
};
use tracing::warn;

use crate::BaseApiServer;

/// Server implementation of the public [`crate::BaseApiServer`] (`base` namespace).
///
/// Holds the node's upgrade-signal configuration and a contract reader so it can answer readiness
/// queries with a fresh, authoritative L1 read (which also confirms the node can reach the contract).
/// It is read-only and never mutates node state, so it is safe to expose on the public RPC.
#[derive(Debug, Clone)]
pub struct BaseRpc {
    /// Upgrade-signal schedule read configuration (carries this node's advertised protocol version).
    pub upgrade_signal_config: UpgradeSignalConfig,
    /// Hardened L1 contract reader built from `upgrade_signal_config`.
    pub upgrade_signal_reader: AlloyUpgradeSignalReader,
}

impl BaseRpc {
    /// Creates a new `base`-namespace RPC server from the node's upgrade-signal config and reader.
    pub const fn new(
        upgrade_signal_config: UpgradeSignalConfig,
        upgrade_signal_reader: AlloyUpgradeSignalReader,
    ) -> Self {
        Self { upgrade_signal_config, upgrade_signal_reader }
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
            .map_err(|error| {
                ErrorObject::owned(ErrorCode::InvalidParams.code(), error.to_string(), None::<()>)
            })
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

#[async_trait]
impl BaseApiServer for BaseRpc {
    async fn upgrade_readiness(
        &self,
        target_version: Option<String>,
    ) -> RpcResult<UpgradeReadiness> {
        Metrics::rpc_calls("base_upgradeReadiness").increment(1.0);

        let target = Self::parse_target_version(target_version)?;
        let now_secs = UpgradeSignalDefaults::now_secs();

        // Fresh, authoritative read from the same contract and block tag the node uses. An empty
        // contract is a valid pre-schedule state (the operator is likely checking a `target_version`
        // ahead of #4), so it is reported as "nothing scheduled" rather than surfaced as an error.
        match self
            .upgrade_signal_config
            .read_schedule(
                &self.upgrade_signal_reader,
                "upgrade readiness",
                &[UpgradeSignalMetricLayer::Consensus],
            )
            .await
        {
            Ok(schedule) => Ok(self.upgrade_signal_config.evaluate_readiness(
                &schedule.signals,
                Some(schedule.l1_block_number),
                now_secs,
                target,
            )),
            Err(UpgradeSignalError::EmptySchedule) => {
                Ok(self.upgrade_signal_config.evaluate_readiness(&[], None, now_secs, target))
            }
            Err(error) => {
                warn!(
                    target: "upgrade_signal",
                    error = %error,
                    "failed to read L1 upgrade schedule for readiness query"
                );
                Err(ErrorObject::owned(
                    -32006,
                    "failed to read L1 upgrade schedule",
                    Some(error.to_string()),
                ))
            }
        }
    }
}
