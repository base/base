use alloy_primitives::U256;
use base_common_genesis::{RuntimeUpgradeRegistry, UpgradeActivation, UpgradeActivationSink};

use super::{RuntimeRegistrySink, UpgradeSignalRuntimeApplier};
use crate::{UpgradeSignal, UpgradeSignalSchedule};

fn schedule(signals: &[(&str, u64)]) -> UpgradeSignalSchedule {
    UpgradeSignalSchedule::new(
        signals
            .iter()
            .map(|(hardfork_id, activation_timestamp)| UpgradeSignal {
                hardfork_id: hardfork_id.to_string(),
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
        &schedule(&[("azul", 42), ("beryl", 0), ("unknown", 10)]),
    );

    assert_eq!(summary.applied_hardforks, 1);
    assert_eq!(summary.cleared_hardforks, 1);
    assert_eq!(summary.ignored_hardforks, 1);
    assert_eq!(
        RuntimeUpgradeRegistry::activation(chain_id, "azul"),
        Some(UpgradeActivation::Timestamp(42))
    );
    assert_eq!(
        RuntimeUpgradeRegistry::activation(chain_id, "beryl"),
        Some(UpgradeActivation::Never)
    );
    assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, "unknown"), None);

    RuntimeUpgradeRegistry::clear_chain(chain_id);
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct RecordingSink {
    applied: Vec<(String, UpgradeActivation)>,
    fail_on_hardfork_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct RecordingSinkError;

impl UpgradeActivationSink for RecordingSink {
    type Error = RecordingSinkError;

    fn apply_activation(
        &mut self,
        hardfork_id: &str,
        activation: UpgradeActivation,
    ) -> Result<bool, Self::Error> {
        if self.fail_on_hardfork_id.as_deref() == Some(hardfork_id) {
            return Err(RecordingSinkError);
        }

        self.applied.push((hardfork_id.to_string(), activation));
        Ok(true)
    }
}

#[test]
fn apply_schedule_to_sink_is_transactional() {
    let mut sink = RecordingSink {
        applied: vec![("existing".to_string(), UpgradeActivation::Timestamp(1))],
        fail_on_hardfork_id: Some("beryl".to_string()),
    };

    let error = UpgradeSignalRuntimeApplier::apply_schedule_to_sink(
        9_000_007,
        &schedule(&[("azul", 42), ("beryl", 84)]),
        &mut sink,
    )
    .unwrap_err();

    assert_eq!(error, RecordingSinkError);
    assert_eq!(sink.applied, vec![("existing".to_string(), UpgradeActivation::Timestamp(1))]);
}

#[test]
fn runtime_registry_sink_only_flushes_in_finalize() {
    let chain_id = 9_000_008;
    RuntimeUpgradeRegistry::clear_chain(chain_id);
    let mut sink = RuntimeRegistrySink::new(chain_id);

    sink.apply_activation("azul", UpgradeActivation::Timestamp(42)).unwrap();

    assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, "azul"), None);

    sink.finalize().unwrap();

    assert_eq!(
        RuntimeUpgradeRegistry::activation(chain_id, "azul"),
        Some(UpgradeActivation::Timestamp(42))
    );

    RuntimeUpgradeRegistry::clear_chain(chain_id);
}

#[test]
fn runtime_registry_sink_replaces_existing_overrides() {
    let chain_id = 9_000_009;
    RuntimeUpgradeRegistry::clear_chain(chain_id);
    RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, "cobalt", 84);

    let mut sink = RuntimeRegistrySink::new(chain_id);
    sink.apply_activation("azul", UpgradeActivation::Timestamp(42)).unwrap();
    sink.finalize().unwrap();

    assert_eq!(
        RuntimeUpgradeRegistry::activation(chain_id, "azul"),
        Some(UpgradeActivation::Timestamp(42))
    );
    assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, "cobalt"), None);

    RuntimeUpgradeRegistry::clear_chain(chain_id);
}
