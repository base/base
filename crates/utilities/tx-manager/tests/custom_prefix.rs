//! Integration tests for `define_tx_manager_cli!` macro with a custom prefix.

use std::time::Duration;

use clap::{CommandFactory, Parser};

base_tx_manager::define_tx_manager_cli!("CUSTOM_PREFIX_");

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
fn defaults_are_unchanged() {
    let cli = TestCli::parse_from(["test"]);
    assert_eq!(cli.tx.num_confirmations, 10);
    assert_eq!(cli.tx.safe_abort_nonce_too_low_count, 3);
    assert_eq!(cli.tx.fee_limit_multiplier, 5);
    assert_eq!(cli.tx.fee_limit_threshold_gwei, "100");
    assert_eq!(cli.tx.min_tip_cap_gwei, "0");
    assert_eq!(cli.tx.min_basefee_gwei, "0");
    assert_eq!(cli.tx.network_timeout, Duration::from_secs(10));
    assert_eq!(cli.tx.resubmission_timeout, Duration::from_secs(48));
    assert_eq!(cli.tx.receipt_query_interval, Duration::from_secs(12));
    assert_eq!(cli.tx.tx_send_timeout, Duration::ZERO);
    assert_eq!(cli.tx.tx_not_in_mempool_timeout, Duration::from_secs(120));
}

#[test]
fn into_params_works() {
    let cli = TestCli::parse_from(["test"]);
    let params = cli.tx.into_params(1).expect("default params should be valid");
    assert_eq!(params.num_confirmations, 10);
    assert_eq!(params.chain_id, 1);
    // 100 gwei = 100_000_000_000 wei
    assert_eq!(params.fee_limit_threshold, 100_000_000_000);
    assert_eq!(params.min_tip_cap, 0);
    assert_eq!(params.min_basefee, 0);
}
