use alloy_primitives::{U256, address};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_common_genesis::HardForkConfig;
use rstest::rstest;

use super::*;
use crate::state::{UpgradeSignal, UpgradeSignalSchedule};

#[test]
fn disabled_when_no_contract_or_hardfork_id() {
    let args = UpgradeSignalArgs::default();

    assert_eq!(args.config().unwrap(), None);
}

#[test]
fn uses_default_ids_for_contract_without_hardfork_id() {
    let args = UpgradeSignalArgs {
        contract_address: Some(address!("0000000000000000000000000000000000000001")),
        ..Default::default()
    };

    let config = args.config().unwrap().unwrap();

    assert_eq!(config.hardfork_ids, HardForkConfig::CONTRACT_HARDFORK_IDS);
    assert_eq!(config.apply_hardfork_ids, HardForkConfig::CONTRACT_HARDFORK_IDS);
    assert_eq!(config.mode, UpgradeSignalMode::MetricsOnly);
}

#[test]
fn rejects_hardfork_id_without_contract() {
    let args = UpgradeSignalArgs { hardfork_ids: vec!["azul".to_string()], ..Default::default() };

    assert!(matches!(args.config().unwrap_err(), UpgradeSignalConfigError::MissingContractAddress));
}

#[rstest]
#[case("azul")]
#[case("beryl")]
fn defaults_to_finalized_block_tag(#[case] hardfork_id: &str) {
    let config =
        UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), hardfork_id);

    assert_eq!(config.l1_block_tag, BlockNumberOrTag::Finalized);
}

#[test]
fn maps_configured_block_tag() {
    let args = UpgradeSignalArgs {
        contract_address: Some(address!("0000000000000000000000000000000000000001")),
        l1_block_tag: UpgradeSignalBlockTag::Latest,
        ..Default::default()
    };

    assert_eq!(args.config().unwrap().unwrap().l1_block_tag, BlockNumberOrTag::Latest);
}

#[test]
fn builds_enabled_config() {
    let contract = address!("0000000000000000000000000000000000000001");
    let args = UpgradeSignalArgs {
        contract_address: Some(contract),
        hardfork_ids: vec!["azul".to_string()],
        mode: UpgradeSignalMode::StartupApply,
        ..Default::default()
    };

    let config = args.config().unwrap().unwrap();

    assert_eq!(config.contract_address, contract);
    assert_eq!(config.hardfork_ids, ["azul"]);
    assert_eq!(config.apply_hardfork_ids, ["azul"]);
    assert_eq!(config.mode, UpgradeSignalMode::StartupApply);
    assert_eq!(
        config.node_protocol_version,
        U256::from(UpgradeSignalDefaults::NODE_PROTOCOL_VERSION)
    );
}

#[test]
fn uses_explicit_apply_ids() {
    let args = UpgradeSignalArgs {
        contract_address: Some(address!("0000000000000000000000000000000000000001")),
        hardfork_ids: vec!["azul".to_string(), "beryl".to_string()],
        apply_hardfork_ids: vec!["beryl".to_string()],
        mode: UpgradeSignalMode::RuntimeAdmin,
        ..Default::default()
    };

    let config = args.config().unwrap().unwrap();

    assert_eq!(config.hardfork_ids, ["azul", "beryl"]);
    assert_eq!(config.apply_hardfork_ids, ["beryl"]);
    assert_eq!(config.mode, UpgradeSignalMode::RuntimeAdmin);
}

#[test]
fn rejects_apply_id_not_in_read_ids() {
    let args = UpgradeSignalArgs {
        contract_address: Some(address!("0000000000000000000000000000000000000001")),
        hardfork_ids: vec!["azul".to_string()],
        apply_hardfork_ids: vec!["beryl".to_string()],
        ..Default::default()
    };

    assert!(matches!(
        args.config().unwrap_err(),
        UpgradeSignalConfigError::ApplyHardforkIdNotRead(_)
    ));
}

fn signal(protocol_version: U256) -> UpgradeSignal {
    UpgradeSignal {
        hardfork_id: "azul".to_string(),
        activation_timestamp: 42,
        protocol_version,
        l1_block_number: 1,
    }
}

#[rstest]
#[case("azul")]
#[case("beryl")]
fn accepts_signal_at_node_protocol_version(#[case] hardfork_id: &str) {
    let config =
        UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), hardfork_id);

    assert!(config.validate_signal_protocol_version(&signal(config.node_protocol_version)).is_ok());
}

#[rstest]
#[case("azul")]
#[case("beryl")]
fn rejects_signal_above_node_protocol_version(#[case] hardfork_id: &str) {
    let config =
        UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), hardfork_id);
    let minimum_protocol_version = config.node_protocol_version + U256::from(1);

    assert!(matches!(
        config.validate_signal_protocol_version(&signal(minimum_protocol_version)).unwrap_err(),
        crate::UpgradeSignalError::UnsupportedProtocolVersion { .. }
    ));
}

#[rstest]
#[case("azul")]
#[case("beryl")]
fn rejects_positive_signal_without_protocol_version(#[case] hardfork_id: &str) {
    let config =
        UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), hardfork_id);

    assert!(matches!(
        config.validate_signal_protocol_version(&signal(U256::ZERO)).unwrap_err(),
        crate::UpgradeSignalError::MissingProtocolVersion(_)
    ));
}

fn malformed_read_only_schedule(config: &UpgradeSignalConfig) -> UpgradeSignalSchedule {
    UpgradeSignalSchedule::new(vec![
        signal(config.node_protocol_version),
        UpgradeSignal {
            hardfork_id: "beryl".to_string(),
            activation_timestamp: 5,
            protocol_version: U256::ZERO,
            l1_block_number: 1,
        },
    ])
}

#[test]
fn read_validation_rejects_missing_protocol_version_on_read_only_fork() {
    let config =
        UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), "azul");
    let schedule = malformed_read_only_schedule(&config);

    assert!(matches!(
        config.validate_read_schedule_protocol_versions(&schedule).unwrap_err(),
        crate::UpgradeSignalError::MissingProtocolVersion(_)
    ));
}

#[test]
fn applied_validation_allows_unsupported_version_on_read_only_fork() {
    let config =
        UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), "azul");

    let schedule = UpgradeSignalSchedule::new(vec![
        UpgradeSignal {
            hardfork_id: "azul".to_string(),
            activation_timestamp: 42,
            protocol_version: config.node_protocol_version,
            l1_block_number: 1,
        },
        UpgradeSignal {
            hardfork_id: "beryl".to_string(),
            activation_timestamp: 42,
            protocol_version: config.node_protocol_version + U256::from(1),
            l1_block_number: 1,
        },
    ]);

    assert!(
        config
            .validate_applied_schedule_protocol_versions(&config.application_schedule(&schedule))
            .is_ok()
    );
}

#[rstest]
#[case("azul")]
#[case("beryl")]
fn applied_validation_allows_clear_with_unsupported_protocol_version(#[case] hardfork_id: &str) {
    let config =
        UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), hardfork_id);
    let schedule = UpgradeSignalSchedule::new(vec![UpgradeSignal {
        hardfork_id: hardfork_id.to_string(),
        activation_timestamp: 0,
        protocol_version: config.node_protocol_version + U256::from(1),
        l1_block_number: 1,
    }]);

    assert!(config.validate_applied_schedule_protocol_versions(&schedule).is_ok());
}
