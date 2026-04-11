//! Integration tests for `define_signer_cli!` macro with a custom prefix.

use clap::{CommandFactory, Parser};

base_tx_manager::define_signer_cli!("TEST_SIGNER");

const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const TEST_ADDRESS: &str = "0x1234567890123456789012345678901234567890";
const TEST_ENDPOINT: &str = "http://localhost:8546";

#[derive(Parser)]
struct TestCli {
    #[command(flatten)]
    signer: SignerCli,
}

#[test]
fn env_vars_use_custom_prefix() {
    let cmd = TestCli::command();

    let cases = [
        ("private-key", "TEST_SIGNER_PRIVATE_KEY"),
        ("signer-endpoint", "TEST_SIGNER_SIGNER_ENDPOINT"),
        ("signer-address", "TEST_SIGNER_SIGNER_ADDRESS"),
    ];

    for (long_name, expected_env) in cases {
        let arg = cmd
            .get_arguments()
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
fn local_signer_accepts_key_with_and_without_0x() {
    let prefixed = format!("0x{TEST_KEY}");
    for key in [prefixed.as_str(), TEST_KEY] {
        let cli = TestCli::try_parse_from(["test", "--private-key", key]).unwrap();
        let config = base_tx_manager::SignerConfig::try_from(cli.signer).unwrap();
        assert!(matches!(config, base_tx_manager::SignerConfig::Local(..)));
    }
}

#[test]
fn remote_signer() {
    let cli = TestCli::try_parse_from([
        "test",
        "--signer-endpoint",
        TEST_ENDPOINT,
        "--signer-address",
        TEST_ADDRESS,
    ])
    .unwrap();

    let config = base_tx_manager::SignerConfig::try_from(cli.signer).unwrap();
    assert!(matches!(config, base_tx_manager::SignerConfig::Remote { .. }));
}

#[test]
fn no_signer_returns_error() {
    let cli = TestCli::try_parse_from(["test"]).unwrap();
    let err = base_tx_manager::SignerConfig::try_from(cli.signer).unwrap_err();
    assert!(matches!(err, base_tx_manager::ConfigError::InvalidValue { field: "signer", .. }));
}

#[test]
fn conflicting_args_rejected_by_clap() {
    let result = TestCli::try_parse_from([
        "test",
        "--private-key",
        TEST_KEY,
        "--signer-endpoint",
        TEST_ENDPOINT,
        "--signer-address",
        TEST_ADDRESS,
    ]);
    assert!(result.is_err(), "clap should reject conflicting args");
}

#[test]
fn endpoint_without_address_rejected_by_clap() {
    let result = TestCli::try_parse_from(["test", "--signer-endpoint", TEST_ENDPOINT]);
    assert!(result.is_err(), "clap should reject endpoint without address");
}

#[test]
fn address_without_endpoint_rejected_by_clap() {
    let result = TestCli::try_parse_from(["test", "--signer-address", TEST_ADDRESS]);
    assert!(result.is_err(), "clap should reject address without endpoint");
}

#[test]
fn invalid_hex_returns_config_error() {
    let cli = TestCli::try_parse_from(["test", "--private-key", "not-a-hex-string"]).unwrap();
    let result = base_tx_manager::SignerConfig::try_from(cli.signer);
    assert!(matches!(
        result.unwrap_err(),
        base_tx_manager::ConfigError::InvalidValue { field: "private-key", .. }
    ));
}

#[test]
fn endpoint_without_host_returns_config_error() {
    let cli = TestCli::try_parse_from([
        "test",
        "--signer-endpoint",
        "file:///some/path",
        "--signer-address",
        TEST_ADDRESS,
    ])
    .unwrap();

    let result = base_tx_manager::SignerConfig::try_from(cli.signer);
    assert!(matches!(
        result.unwrap_err(),
        base_tx_manager::ConfigError::InvalidValue { field: "signer-endpoint", .. }
    ));
}

#[test]
fn debug_redacts_private_key() {
    let cli = TestCli::try_parse_from(["test", "--private-key", TEST_KEY]).unwrap();

    let debug_output = format!("{:?}", cli.signer);
    assert!(
        debug_output.contains("[REDACTED]"),
        "debug output should contain [REDACTED], got: {debug_output}"
    );
    assert!(
        !debug_output.contains(TEST_KEY),
        "debug output should not contain the key, got: {debug_output}"
    );
}
