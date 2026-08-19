use base_common_genesis::{
    BaseUpgrade, RuntimeUpgradeRegistry, UpgradeActivation, UpgradeActivationOverrides,
    UpgradeActivationSink, UpgradeConfig,
};
use tracing::warn;

use super::{UpgradeSignalApplyAction, UpgradeSignalApplyChange, UpgradeSignalApplySummary};
use crate::{UpgradeSignalMetrics, UpgradeSignalSchedule};

/// Upgrade activation sink backed by the process-local runtime registry for one chain.
#[derive(Debug, Clone)]
pub struct RuntimeRegistrySink {
    /// L2 chain ID whose runtime upgrade view is mutated.
    pub chain_id: u64,
    /// L1 block number used to read the buffered schedule.
    pub l1_block_number: u64,
    /// Buffered updates to apply to the runtime registry at finalize time.
    pub updates: UpgradeActivationOverrides,
}

impl RuntimeRegistrySink {
    /// Creates a runtime registry sink for one chain.
    pub const fn new(chain_id: u64, l1_block_number: u64) -> Self {
        Self { chain_id, l1_block_number, updates: UpgradeActivationOverrides::new() }
    }
}

impl UpgradeActivationSink for RuntimeRegistrySink {
    type Error = core::convert::Infallible;

    fn apply_activation(
        &mut self,
        upgrade_id: BaseUpgrade,
        activation: UpgradeActivation,
    ) -> Result<bool, Self::Error> {
        if matches!(upgrade_id, BaseUpgrade::Zenith) {
            return Ok(false);
        }

        self.updates.set_activation(upgrade_id, activation);
        Ok(true)
    }

    fn finalize(&mut self) -> Result<bool, Self::Error> {
        let updates = core::mem::take(&mut self.updates);

        // The runtime registry mirrors the latest authoritative L1 schedule for this chain, so a
        // refresh replaces the chain's entire override set instead of merging into prior state.
        Ok(RuntimeUpgradeRegistry::replace_overrides(self.chain_id, self.l1_block_number, updates))
    }
}

/// Applies upgrade signal schedules to any upgrade activation sink.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeSignalRuntimeApplier;

impl UpgradeSignalRuntimeApplier {
    /// Applies a schedule to any [`UpgradeActivationSink`], returning an application summary.
    ///
    /// This stages the full batch on a cloned sink and only commits it back on success, so a
    /// failed later activation cannot leave earlier mutations partially applied.
    pub fn apply_schedule_to_sink<S: UpgradeActivationSink + Clone>(
        chain_id: u64,
        schedule: &UpgradeSignalSchedule,
        sink: &mut S,
    ) -> Result<UpgradeSignalApplySummary, S::Error> {
        let mut summary = UpgradeSignalApplySummary::new(chain_id, schedule);
        let mut staged_sink = sink.clone();

        // Normalize the cascade ladder before applying: a later fork always drags its predecessors
        // with it. Without this, a schedule that clears a predecessor (e.g. Canyon) while a
        // successor (e.g. Ecotone) stays scheduled would make the EL (independent per-fork) disagree
        // with the CL (cascading), splitting activation across the two layers. All runtime paths
        // (execution chain spec, rollup config, runtime registry) flow through this method, so
        // filling here keeps every consumer consistent. `positive_activation_timestamp` maps an
        // absent/zero activation to `Never`, so raw signals cleared on L1 stay cleared unless a
        // scheduled successor pulls them forward.
        let mut normalized = UpgradeConfig::default();
        for signal in &schedule.signals {
            normalized.set_activation(
                signal.upgrade_id,
                UpgradeActivation::from_timestamp(signal.positive_activation_timestamp()),
            );
        }
        for (upgrade_id, filled_timestamp) in normalized.normalize_cascade_ladder() {
            warn!(
                chain_id,
                upgrade = %upgrade_id.contract_id(),
                filled_timestamp,
                "filled upgrade schedule hole to keep CL/EL activation consistent"
            );
            UpgradeSignalMetrics::record_hole_filled(upgrade_id);
        }

        for signal in &schedule.signals {
            // Report the raw L1 signal in the summary, but apply the normalized (hole-filled)
            // activation to the sink so the sink's schedule cannot contain a cascade hole.
            let activation =
                UpgradeActivation::from_timestamp(signal.positive_activation_timestamp());
            let applied_activation = normalized.activation(signal.upgrade_id);
            let supported = staged_sink.apply_activation(signal.upgrade_id, applied_activation)?;

            let action = if !supported {
                UpgradeSignalApplyAction::Ignored
            } else if activation.timestamp().is_some() {
                UpgradeSignalApplyAction::Applied
            } else {
                UpgradeSignalApplyAction::Cleared
            };
            match action {
                UpgradeSignalApplyAction::Applied => summary.applied_upgrades += 1,
                UpgradeSignalApplyAction::Cleared => summary.cleared_upgrades += 1,
                UpgradeSignalApplyAction::Ignored => summary.ignored_upgrades += 1,
            }

            summary.changes.push(UpgradeSignalApplyChange {
                upgrade_id: signal.upgrade_id.contract_id().to_string(),
                action,
                activation_timestamp: signal.activation_timestamp,
                minimum_protocol_version: signal.protocol_version.to_string(),
                l1_block_number: schedule.l1_block_number,
            });
        }

        summary.committed = staged_sink.finalize()?;
        if summary.committed {
            *sink = staged_sink;
        }

        Ok(summary)
    }

    /// Applies a schedule to the runtime upgrade registry for one chain.
    pub fn apply_schedule(
        chain_id: u64,
        schedule: &UpgradeSignalSchedule,
    ) -> UpgradeSignalApplySummary {
        let mut sink = RuntimeRegistrySink::new(chain_id, schedule.l1_block_number);
        Self::apply_schedule_to_sink(chain_id, schedule, &mut sink)
            .unwrap_or_else(|never| match never {})
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use base_common_genesis::{
        BaseUpgrade, RuntimeUpgradeRegistry, UpgradeActivation, UpgradeActivationSink,
    };

    use super::{RuntimeRegistrySink, UpgradeSignalRuntimeApplier};
    use crate::{UpgradeSignal, UpgradeSignalSchedule};

    fn schedule_at(l1_block_number: u64, signals: &[(BaseUpgrade, u64)]) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            l1_block_number,
            signals
                .iter()
                .map(|(upgrade_id, activation_timestamp)| UpgradeSignal {
                    upgrade_id: *upgrade_id,
                    activation_timestamp: *activation_timestamp,
                    protocol_version: U256::from(7),
                })
                .collect(),
        )
    }

    fn schedule(signals: &[(BaseUpgrade, u64)]) -> UpgradeSignalSchedule {
        schedule_at(11, signals)
    }

    #[test]
    fn applies_runtime_schedule() {
        let chain_id = 9_000_001;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        // A well-formed schedule: Azul<Beryl scheduled, Cobalt (the latest of the three) cleared.
        // Clearing the tail leaves no hole, so normalization is a no-op here.
        let summary = UpgradeSignalRuntimeApplier::apply_schedule(
            chain_id,
            &schedule(&[
                (BaseUpgrade::Azul, 42),
                (BaseUpgrade::Beryl, 84),
                (BaseUpgrade::Cobalt, 0),
            ]),
        );

        assert_eq!(summary.applied_upgrades, 2);
        assert_eq!(summary.cleared_upgrades, 1);
        assert_eq!(summary.ignored_upgrades, 0);
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Beryl),
            Some(UpgradeActivation::Timestamp(84))
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Cobalt),
            Some(UpgradeActivation::Never)
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[derive(Debug, Clone, Default, Eq, PartialEq)]
    struct RecordingSink {
        applied: Vec<(BaseUpgrade, UpgradeActivation)>,
        fail_on_upgrade_id: Option<BaseUpgrade>,
    }

    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    struct RecordingSinkError;

    impl UpgradeActivationSink for RecordingSink {
        type Error = RecordingSinkError;

        fn apply_activation(
            &mut self,
            upgrade_id: BaseUpgrade,
            activation: UpgradeActivation,
        ) -> Result<bool, Self::Error> {
            if self.fail_on_upgrade_id == Some(upgrade_id) {
                return Err(RecordingSinkError);
            }

            self.applied.push((upgrade_id, activation));
            Ok(true)
        }
    }

    #[test]
    fn apply_schedule_to_sink_is_transactional() {
        let mut sink = RecordingSink {
            applied: vec![(BaseUpgrade::Regolith, UpgradeActivation::Timestamp(1))],
            fail_on_upgrade_id: Some(BaseUpgrade::Beryl),
        };

        let error = UpgradeSignalRuntimeApplier::apply_schedule_to_sink(
            9_000_007,
            &schedule(&[(BaseUpgrade::Azul, 42), (BaseUpgrade::Beryl, 84)]),
            &mut sink,
        )
        .unwrap_err();

        assert_eq!(error, RecordingSinkError);
        assert_eq!(sink.applied, vec![(BaseUpgrade::Regolith, UpgradeActivation::Timestamp(1))]);
    }

    #[test]
    fn applies_filled_activations_when_schedule_has_a_cascade_hole() {
        // A hole: Canyon and Delta cleared (timestamp 0 => Never) while Ecotone stays at 100.
        let mut sink = RecordingSink::default();

        let summary = UpgradeSignalRuntimeApplier::apply_schedule_to_sink(
            9_000_100,
            &schedule(&[
                (BaseUpgrade::Regolith, 1),
                (BaseUpgrade::Canyon, 0),
                (BaseUpgrade::Delta, 0),
                (BaseUpgrade::Ecotone, 100),
            ]),
            &mut sink,
        )
        .unwrap();

        assert!(summary.committed);
        // The sink receives Canyon and Delta filled forward to Ecotone, not the raw cleared values,
        // so an independent-reading sink (the EL fork table) cannot end up with a cascade hole.
        assert_eq!(
            sink.applied,
            vec![
                (BaseUpgrade::Regolith, UpgradeActivation::Timestamp(1)),
                (BaseUpgrade::Canyon, UpgradeActivation::Timestamp(100)),
                (BaseUpgrade::Delta, UpgradeActivation::Timestamp(100)),
                (BaseUpgrade::Ecotone, UpgradeActivation::Timestamp(100)),
            ]
        );
        // The summary still reports the raw L1 signals: Canyon and Delta were cleared on L1.
        assert_eq!(summary.cleared_upgrades, 2);
        assert_eq!(summary.applied_upgrades, 2);
    }

    #[test]
    fn leaves_wellformed_schedule_unchanged() {
        let mut sink = RecordingSink::default();

        UpgradeSignalRuntimeApplier::apply_schedule_to_sink(
            9_000_101,
            &schedule(&[
                (BaseUpgrade::Regolith, 1),
                (BaseUpgrade::Canyon, 2),
                (BaseUpgrade::Delta, 3),
                (BaseUpgrade::Ecotone, 4),
            ]),
            &mut sink,
        )
        .unwrap();

        assert_eq!(
            sink.applied,
            vec![
                (BaseUpgrade::Regolith, UpgradeActivation::Timestamp(1)),
                (BaseUpgrade::Canyon, UpgradeActivation::Timestamp(2)),
                (BaseUpgrade::Delta, UpgradeActivation::Timestamp(3)),
                (BaseUpgrade::Ecotone, UpgradeActivation::Timestamp(4)),
            ]
        );
    }

    #[test]
    fn runtime_registry_sink_only_flushes_in_finalize() {
        let chain_id = 9_000_008;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let mut sink = RuntimeRegistrySink::new(chain_id, 11);

        sink.apply_activation(BaseUpgrade::Azul, UpgradeActivation::Timestamp(42)).unwrap();

        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul), None);

        assert!(sink.finalize().unwrap());

        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn runtime_registry_sink_replaces_existing_overrides() {
        let chain_id = 9_000_009;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Cobalt, 84);

        let mut sink = RuntimeRegistrySink::new(chain_id, 11);
        sink.apply_activation(BaseUpgrade::Azul, UpgradeActivation::Timestamp(42)).unwrap();
        assert!(sink.finalize().unwrap());

        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );
        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Cobalt), None);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn stale_schedule_does_not_replace_newer_runtime_overrides() {
        let chain_id = 9_000_010;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let newer_summary = UpgradeSignalRuntimeApplier::apply_schedule(
            chain_id,
            &schedule_at(101, &[(BaseUpgrade::Jovian, 200)]),
        );
        let stale_summary = UpgradeSignalRuntimeApplier::apply_schedule(
            chain_id,
            &schedule_at(100, &[(BaseUpgrade::Jovian, 100)]),
        );

        assert!(newer_summary.committed);
        assert!(!stale_summary.committed);
        assert_eq!(RuntimeUpgradeRegistry::last_updated_block_number(chain_id), Some(101));
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Jovian),
            Some(UpgradeActivation::Timestamp(200))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn newer_schedule_replaces_older_runtime_overrides() {
        let chain_id = 9_000_011;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let older_summary = UpgradeSignalRuntimeApplier::apply_schedule(
            chain_id,
            &schedule_at(100, &[(BaseUpgrade::Jovian, 100)]),
        );
        let newer_summary = UpgradeSignalRuntimeApplier::apply_schedule(
            chain_id,
            &schedule_at(101, &[(BaseUpgrade::Jovian, 200)]),
        );

        assert!(older_summary.committed);
        assert!(newer_summary.committed);
        assert_eq!(RuntimeUpgradeRegistry::last_updated_block_number(chain_id), Some(101));
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Jovian),
            Some(UpgradeActivation::Timestamp(200))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn empty_schedule_preserves_ordering_watermark() {
        let chain_id = 9_000_012;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        UpgradeSignalRuntimeApplier::apply_schedule(
            chain_id,
            &schedule_at(100, &[(BaseUpgrade::Jovian, 200)]),
        );
        let empty_summary = UpgradeSignalRuntimeApplier::apply_schedule(
            chain_id,
            &UpgradeSignalSchedule::new(101, Vec::new()),
        );
        let stale_summary = UpgradeSignalRuntimeApplier::apply_schedule(
            chain_id,
            &schedule_at(100, &[(BaseUpgrade::Jovian, 100)]),
        );

        assert!(empty_summary.committed);
        assert!(!stale_summary.committed);
        assert_eq!(RuntimeUpgradeRegistry::last_updated_block_number(chain_id), Some(101));
        assert_eq!(RuntimeUpgradeRegistry::overrides(chain_id), Some(Default::default()));

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }
}
