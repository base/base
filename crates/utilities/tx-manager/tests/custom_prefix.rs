#![cfg(feature = "cli")]
//! Integration tests for `define_tx_manager_cli!` macro with a custom prefix.

use clap::{CommandFactory, Parser};

base_tx_manager::define_tx_manager_cli!("CUSTOM_PREFIX");

#[derive(Parser)]
struct TestCli {
    #[command(flatten)]
    tx: TxManagerCli,
}

#[test]
fn env_vars_use_custom_prefix() {
    let cmd = TestCli::command();
    let args: Vec<_> = cmd.get_arguments().collect();

    let cases = [
        ("tx-manager.num-confirmations", "CUSTOM_PREFIX_NUM_CONFIRMATIONS"),
        (
            "tx-manager.safe-abort-nonce-too-low-count",
            "CUSTOM_PREFIX_SAFE_ABORT_NONCE_TOO_LOW_COUNT",
        ),
        ("tx-manager.fee-limit-multiplier", "CUSTOM_PREFIX_FEE_LIMIT_MULTIPLIER"),
        ("tx-manager.fee-limit-threshold", "CUSTOM_PREFIX_FEE_LIMIT_THRESHOLD"),
        ("tx-manager.min-tip-cap", "CUSTOM_PREFIX_MIN_TIP_CAP"),
        ("tx-manager.min-basefee", "CUSTOM_PREFIX_MIN_BASEFEE"),
        ("tx-manager.network-timeout", "CUSTOM_PREFIX_NETWORK_TIMEOUT"),
        ("tx-manager.resubmission-timeout", "CUSTOM_PREFIX_RESUBMISSION_TIMEOUT"),
        ("tx-manager.receipt-query-interval", "CUSTOM_PREFIX_RECEIPT_QUERY_INTERVAL"),
        ("tx-manager.tx-send-timeout", "CUSTOM_PREFIX_TX_SEND_TIMEOUT"),
        ("tx-manager.tx-not-in-mempool-timeout", "CUSTOM_PREFIX_TX_NOT_IN_MEMPOOL_TIMEOUT"),
    ];

    for (long_name, expected_env) in cases {
        let arg = args
            .iter()
            .find(|a| a.get_long() == Some(long_name))
            .unwrap_or_else(|| panic!("{long_name} arg should exist"));
        assert_eq!(
            arg.get_env().map(|s| s.to_str().unwrap()),
            Some(expected_env),
            "env var for {long_name} should use custom prefix"
        );
    }
}

#[test]
fn try_from_default_matches_config_default() {
    let config = base_tx_manager::TxManagerConfig::try_from(TxManagerCli::default())
        .expect("default CLI should convert successfully");
    assert_eq!(config, base_tx_manager::TxManagerConfig::default());
}
