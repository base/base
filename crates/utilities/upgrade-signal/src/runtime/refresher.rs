use base_common_genesis::{RuntimeUpgradeRegistry, UpgradeActivation};

use super::{UpgradeSignalApplySummary, UpgradeSignalRuntimeApplier};
use crate::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalDefaults, UpgradeSignalError,
    UpgradeSignalMetricLayer, UpgradeSignalSchedule,
};

/// Reads and applies upgrade signal schedules while the node is running.
#[derive(Debug, Clone)]
pub struct UpgradeSignalRefresher {
    /// Shared upgrade signal schedule read configuration.
    pub config: UpgradeSignalConfig,
    /// L1 upgrade signal reader.
    pub reader: AlloyUpgradeSignalReader,
    /// L2 chain ID whose runtime upgrade view is updated.
    pub chain_id: u64,
    /// Metric layer recorded by this refresher.
    pub metrics_layer: UpgradeSignalMetricLayer,
}

impl UpgradeSignalRefresher {
    /// Creates a runtime upgrade signal refresher.
    pub const fn new(
        config: UpgradeSignalConfig,
        reader: AlloyUpgradeSignalReader,
        chain_id: u64,
        metrics_layer: UpgradeSignalMetricLayer,
    ) -> Self {
        Self { config, reader, chain_id, metrics_layer }
    }

    /// Validates and applies an already-read schedule without touching L1.
    ///
    /// This is atomic: the whole schedule is validated before any registry mutation, so a
    /// validation failure leaves the runtime registry unchanged. The live poller
    /// ([`crate::UpgradeSignalMonitor::poll_and_apply`]) advances its applied baseline only when
    /// this call succeeds, so a failed apply leaves the schedule offered for retry on the next
    /// poll rather than being silently adopted as the baseline.
    ///
    /// `now_secs` is the time the schedule is judged against by
    /// [`Self::reject_retroactive_change`]. Protocol versions are validated first, so a schedule
    /// this binary is too old to support still surfaces as
    /// [`UpgradeSignalError::UnsupportedProtocolVersion`] (and can still fail the node closed)
    /// rather than being masked by a retroactivity rejection.
    pub fn apply(
        &self,
        schedule: &UpgradeSignalSchedule,
        now_secs: u64,
    ) -> Result<UpgradeSignalApplySummary, UpgradeSignalError> {
        self.config.validate_schedule_protocol_versions(schedule)?;
        self.reject_retroactive_change(schedule, now_secs)?;
        let summary = UpgradeSignalRuntimeApplier::apply_schedule(self.chain_id, schedule);
        summary.log("runtime registry");

        Ok(summary)
    }

    /// Returns an error if applying `schedule` would change the fork rules of any L2 block at or
    /// before `now_secs`.
    ///
    /// A runtime override is not a forward-only setting: [`crate::RuntimeRegistrySink`] writes it
    /// into [`RuntimeUpgradeRegistry`], which every fork check consults live for *any* timestamp.
    /// Moving or clearing an activation that has already elapsed therefore reinterprets blocks the
    /// node has already built or validated, and the divergence surfaces later and far from its
    /// cause — as a mismatched state root during replay, reorg handling, or proving. Refusing the
    /// apply keeps the node on one coherent rule set and turns a silent retroactive rewrite into a
    /// loud, deterministic alarm.
    ///
    /// The window an activation change flips is `[min(current, incoming), max(current, incoming))`,
    /// so the change reaches settled history exactly when that window has already opened. A cleared
    /// activation is unbounded, hence [`u64::MAX`]. Equality is treated as retroactive: a block
    /// bearing exactly `now_secs` may already exist.
    ///
    /// Two changes are deliberately *not* refused here, because the registry cannot see what the
    /// node would fall back to:
    ///
    /// * an upgrade with no current override — the effective activation comes from the startup
    ///   chain spec or rollup config, which this crate does not read, so there is no baseline to
    ///   compare against. This is the first live apply after a `startup-apply` boot.
    /// * an upgrade dropped by a shorter schedule — the commit removes the override, reverting the
    ///   fork check to that same unreadable startup value.
    ///
    /// Closing both requires the node's real L2 head and effective fork schedule rather than wall
    /// clock and the registry; until then this guard covers every change to an activation the
    /// registry itself already applies.
    pub fn reject_retroactive_change(
        &self,
        schedule: &UpgradeSignalSchedule,
        now_secs: u64,
    ) -> Result<(), UpgradeSignalError> {
        for signal in &schedule.signals {
            let Some(current) =
                RuntimeUpgradeRegistry::activation(self.chain_id, signal.upgrade_id)
            else {
                continue;
            };
            let incoming =
                UpgradeActivation::from_timestamp(signal.positive_activation_timestamp());
            if current == incoming {
                continue;
            }

            let earliest_affected_timestamp = current
                .timestamp()
                .unwrap_or(u64::MAX)
                .min(incoming.timestamp().unwrap_or(u64::MAX));
            if earliest_affected_timestamp <= now_secs {
                return Err(UpgradeSignalError::retroactive_schedule_change(
                    signal.upgrade_id.contract_id().to_string(),
                    current,
                    incoming,
                    earliest_affected_timestamp,
                    now_secs,
                ));
            }
        }

        Ok(())
    }

    /// Reads the current L1 schedule with retries, recording this refresher's metric layer.
    pub async fn read_schedule(&self) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        self.config.read_schedule(&self.reader, "runtime refresh", &[self.metrics_layer]).await
    }

    /// Reads, metrics-records, logs, and applies the current L1 schedule.
    ///
    /// Validation happens once in [`Self::apply`].
    pub async fn refresh(&self) -> Result<UpgradeSignalApplySummary, UpgradeSignalError> {
        let schedule = self.read_schedule().await?;
        self.apply(&schedule, UpgradeSignalDefaults::now_secs())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_common_genesis::{BaseUpgrade, RuntimeUpgradeRegistry, UpgradeActivation};

    use super::*;
    use crate::{UpgradeSignal, UpgradeSignalDefaults};

    fn refresher(chain_id: u64) -> UpgradeSignalRefresher {
        let config = UpgradeSignalConfig::new(Address::ZERO);
        let reader = config.reader("http://127.0.0.1:1".parse().unwrap()).unwrap();
        UpgradeSignalRefresher::new(config, reader, chain_id, UpgradeSignalMetricLayer::Consensus)
    }

    fn schedule(
        upgrade_id: BaseUpgrade,
        activation_timestamp: u64,
        protocol_version: U256,
    ) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            1,
            vec![UpgradeSignal { upgrade_id, activation_timestamp, protocol_version }],
        )
    }

    #[test]
    fn apply_applies_valid_schedule_to_registry() {
        let chain_id = 9_100_001;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let summary = refresher(chain_id)
            .apply(
                &schedule(BaseUpgrade::Azul, 42, UpgradeSignalDefaults::node_protocol_version()),
                0,
            )
            .unwrap();

        assert_eq!(summary.applied_upgrades, 1);
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn apply_rejects_unsupported_protocol_version_without_mutating_registry() {
        let chain_id = 9_100_002;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        // Node supports 1.1.0; a 1.1.1 minimum is genuinely newer and must be rejected. Adding
        // `+ 1` to the dev-build sentinel no longer works: it decodes to a pre-release of the max
        // release, which now sorts below the release under the semver ordering.
        let mut config = UpgradeSignalConfig::new(Address::ZERO);
        config.node_protocol_version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        let reader = config.reader("http://127.0.0.1:1".parse().unwrap()).unwrap();
        let refresher = UpgradeSignalRefresher::new(
            config,
            reader,
            chain_id,
            UpgradeSignalMetricLayer::Consensus,
        );

        let unsupported = UpgradeSignalDefaults::packed_protocol_version(1, 1, 1);
        refresher.apply(&schedule(BaseUpgrade::Azul, 42, unsupported), 0).unwrap_err();

        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul), None);
    }

    /// A supportable schedule placing Azul at `activation_timestamp`, ready to apply.
    fn supported_schedule(activation_timestamp: u64) -> UpgradeSignalSchedule {
        schedule(
            BaseUpgrade::Azul,
            activation_timestamp,
            UpgradeSignalDefaults::node_protocol_version(),
        )
    }

    /// Seeds the registry with an already-applied Azul activation, as a committed apply would.
    fn seed_registry(chain_id: u64, activation: UpgradeActivation) {
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        RuntimeUpgradeRegistry::set_activation(chain_id, BaseUpgrade::Azul, activation);
    }

    /// The audit's case: an activation the node has already passed is cleared on L1, and the node
    /// only observes the clear afterwards (a stalled L1 finality tag, an RPC outage spanning the
    /// activation, or a late admin refresh). Blocks between the activation and now were built with
    /// Azul active, so adopting the clear would reinterpret them.
    #[test]
    fn apply_refuses_a_clear_of_an_activation_the_node_has_already_passed() {
        let chain_id = 9_100_020;
        seed_registry(chain_id, UpgradeActivation::Timestamp(1_000));

        let error = refresher(chain_id).apply(&supported_schedule(0), 5_000).unwrap_err();

        assert!(matches!(
            error,
            UpgradeSignalError::RetroactiveScheduleChange {
                earliest_affected_timestamp: 1_000,
                ..
            }
        ));
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(1_000)),
            "a refused apply must leave the node's current schedule intact"
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    /// Delaying an elapsed activation is equally retroactive: blocks in `[1_000, now)` were Azul
    /// blocks and would stop being Azul blocks.
    #[test]
    fn apply_refuses_pushing_an_elapsed_activation_into_the_future() {
        let chain_id = 9_100_021;
        seed_registry(chain_id, UpgradeActivation::Timestamp(1_000));

        refresher(chain_id).apply(&supported_schedule(9_000), 5_000).unwrap_err();

        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(1_000))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    /// The mirror image: back-dating an activation onto history the node built unforked.
    #[test]
    fn apply_refuses_back_dating_an_activation_onto_settled_history() {
        let chain_id = 9_100_022;
        seed_registry(chain_id, UpgradeActivation::Never);

        refresher(chain_id).apply(&supported_schedule(1_000), 5_000).unwrap_err();

        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Never)
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    /// The whole point of runtime signalling: a still-future activation may be freely rescheduled,
    /// because no block has been built under either rule yet.
    #[test]
    fn apply_commits_a_reschedule_while_both_activations_are_still_future() {
        let chain_id = 9_100_023;
        seed_registry(chain_id, UpgradeActivation::Timestamp(8_000));

        assert!(refresher(chain_id).apply(&supported_schedule(9_000), 5_000).unwrap().committed);

        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(9_000))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    /// Every poll re-reads the full schedule, including long-elapsed activations. Re-offering an
    /// unchanged elapsed activation changes nothing and must stay applicable, or the guard would
    /// wedge the node the moment its first upgrade activates.
    #[test]
    fn apply_commits_an_unchanged_elapsed_activation() {
        let chain_id = 9_100_024;
        seed_registry(chain_id, UpgradeActivation::Timestamp(1_000));

        assert!(refresher(chain_id).apply(&supported_schedule(1_000), 5_000).unwrap().committed);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    /// Equality is retroactive: a block bearing exactly `now_secs` may already exist, so the guard
    /// refuses at the boundary and only permits the change one second earlier.
    #[test]
    fn apply_treats_the_earliest_affected_timestamp_as_already_settled() {
        let chain_id = 9_100_025;

        seed_registry(chain_id, UpgradeActivation::Timestamp(1_000));
        refresher(chain_id).apply(&supported_schedule(0), 1_000).unwrap_err();

        seed_registry(chain_id, UpgradeActivation::Timestamp(1_000));
        assert!(refresher(chain_id).apply(&supported_schedule(0), 999).unwrap().committed);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    /// Documents the guard's known limit: with no override to compare against, the effective
    /// activation lives in the startup chain spec, which this crate cannot read. The first live
    /// apply after a `startup-apply` boot therefore commits an elapsed activation unchecked.
    #[test]
    fn apply_commits_an_elapsed_activation_when_the_registry_has_no_baseline() {
        let chain_id = 9_100_026;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        assert!(refresher(chain_id).apply(&supported_schedule(1_000), 5_000).unwrap().committed);

        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(1_000))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }
}
