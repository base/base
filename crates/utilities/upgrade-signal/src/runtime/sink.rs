use base_common_genesis::{
    RuntimeUpgradeRegistry, UpgradeActivation, UpgradeActivationOverrides, UpgradeActivationSink,
};

use super::{UpgradeSignalApplyAction, UpgradeSignalApplyChange, UpgradeSignalApplySummary};
use crate::UpgradeSignalSchedule;

/// Upgrade activation sink backed by the process-local runtime registry for one chain.
#[derive(Debug, Clone, Default)]
pub struct RuntimeRegistrySink {
    /// L2 chain ID whose runtime fork view is mutated.
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
        hardfork_id: &str,
        activation: UpgradeActivation,
    ) -> Result<bool, Self::Error> {
        Ok(self.updates.set_activation(hardfork_id, activation))
    }

    fn finalize(&mut self) -> Result<(), Self::Error> {
        let updates = core::mem::take(&mut self.updates);

        RuntimeUpgradeRegistry::update_chain(self.chain_id, |overrides| {
            for (hardfork_id, activation) in updates.activations {
                overrides.set_activation(&hardfork_id, activation);
            }
        });

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
            let supported = staged_sink.apply_activation(&signal.hardfork_id, activation)?;

            let action = if !supported {
                UpgradeSignalApplyAction::Ignored
            } else if activation.timestamp().is_some() {
                UpgradeSignalApplyAction::Applied
            } else {
                UpgradeSignalApplyAction::Cleared
            };
            match action {
                UpgradeSignalApplyAction::Applied => summary.applied_hardforks += 1,
                UpgradeSignalApplyAction::Cleared => summary.cleared_hardforks += 1,
                UpgradeSignalApplyAction::Ignored => summary.ignored_hardforks += 1,
            }

            summary.changes.push(UpgradeSignalApplyChange {
                hardfork_id: signal.hardfork_id.clone(),
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
