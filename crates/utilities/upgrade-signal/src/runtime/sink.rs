use base_common_genesis::{
    BaseUpgrade, RuntimeUpgradeRegistry, UpgradeActivation, UpgradeActivationOverrides,
    UpgradeActivationSink,
};

use super::{UpgradeSignalApplyAction, UpgradeSignalApplyChange, UpgradeSignalApplySummary};
use crate::UpgradeSignalSchedule;

/// Upgrade activation sink backed by the process-local runtime registry for one chain.
#[derive(Debug, Clone, Default)]
pub struct RuntimeRegistrySink {
    /// L2 chain ID whose runtime upgrade view is mutated.
    pub chain_id: u64,
    /// Buffered updates to apply to the runtime registry at finalize time.
    pub updates: UpgradeActivationOverrides,
}

impl RuntimeRegistrySink {
    /// Creates a runtime registry sink for one chain.
    pub const fn new(chain_id: u64) -> Self {
        Self { chain_id, updates: UpgradeActivationOverrides::new() }
    }
}

impl UpgradeActivationSink for RuntimeRegistrySink {
    type Error = core::convert::Infallible;

    fn apply_activation(
        &mut self,
        upgrade_id: BaseUpgrade,
        activation: UpgradeActivation,
    ) -> Result<bool, Self::Error> {
        if matches!(upgrade_id, BaseUpgrade::Zombie) {
            return Ok(false);
        }

        self.updates.set_activation(upgrade_id, activation);
        Ok(true)
    }

    fn finalize(&mut self) -> Result<(), Self::Error> {
        let updates = core::mem::take(&mut self.updates);

        // The runtime registry mirrors the latest authoritative L1 schedule for this chain, so a
        // refresh replaces the chain's entire override set instead of merging into prior state.
        if updates.is_empty() {
            RuntimeUpgradeRegistry::clear_chain(self.chain_id);
        } else {
            RuntimeUpgradeRegistry::replace_overrides(self.chain_id, updates);
        }

        Ok(())
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

        for signal in &schedule.signals {
            let activation =
                UpgradeActivation::from_timestamp(signal.positive_activation_timestamp());
            let supported = staged_sink.apply_activation(signal.upgrade_id, activation)?;

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
                l1_block_number: signal.l1_block_number,
            });
        }

        staged_sink.finalize()?;
        *sink = staged_sink;

        Ok(summary)
    }

    /// Applies a schedule to the runtime upgrade registry for one chain.
    pub fn apply_schedule(
        chain_id: u64,
        schedule: &UpgradeSignalSchedule,
    ) -> UpgradeSignalApplySummary {
        let mut sink = RuntimeRegistrySink::new(chain_id);
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

    fn schedule(signals: &[(BaseUpgrade, u64)]) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            signals
                .iter()
                .map(|(upgrade_id, activation_timestamp)| UpgradeSignal {
                    upgrade_id: *upgrade_id,
                    activation_timestamp: *activation_timestamp,
                    protocol_version: U256::from(7),
                    l1_block_number: 11,
                })
                .collect(),
        )
    }

    #[test]
    fn applies_runtime_schedule() {
        let chain_id = 9_000_001;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let summary = UpgradeSignalRuntimeApplier::apply_schedule(
            chain_id,
            &schedule(&[
                (BaseUpgrade::Azul, 42),
                (BaseUpgrade::Beryl, 0),
                (BaseUpgrade::Cobalt, 10),
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
            Some(UpgradeActivation::Never)
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Cobalt),
            Some(UpgradeActivation::Timestamp(10))
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
    fn runtime_registry_sink_only_flushes_in_finalize() {
        let chain_id = 9_000_008;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let mut sink = RuntimeRegistrySink::new(chain_id);

        sink.apply_activation(BaseUpgrade::Azul, UpgradeActivation::Timestamp(42)).unwrap();

        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul), None);

        sink.finalize().unwrap();

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

        let mut sink = RuntimeRegistrySink::new(chain_id);
        sink.apply_activation(BaseUpgrade::Azul, UpgradeActivation::Timestamp(42)).unwrap();
        sink.finalize().unwrap();

        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );
        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Cobalt), None);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }
}
