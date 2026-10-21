//! Checks that `--help` from a `define_signer_cli!` struct never prints the
//! private key read from its env var.
//!
//! Kept in its own test binary because it sets a process env var, which would
//! leak into the parsing tests in `signer_prefix.rs`.

use clap::{CommandFactory, Parser};

base_tx_manager::define_signer_cli!("TEST_SIGNER_HELP");

const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[derive(Parser)]
struct TestCli {
    #[command(flatten)]
    signer: SignerCli,
}

#[test]
fn help_hides_private_key_env_value() {
    // SAFETY: this is the only test in this binary, so no other thread reads
    // or writes the environment concurrently.
    unsafe { std::env::set_var("TEST_SIGNER_HELP_PRIVATE_KEY", TEST_KEY) };

    let help = TestCli::command().render_help().to_string();

    assert!(help.contains("TEST_SIGNER_HELP_PRIVATE_KEY"), "help should name the env var: {help}");
    assert!(!help.contains(TEST_KEY), "help should not print the private key: {help}");
}
