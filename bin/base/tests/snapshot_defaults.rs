//! Subprocess regressions for Base snapshot download initialization.

use std::process::Command;

use tempfile::TempDir;

#[test]
fn snapshot_download_help_advertises_base_defaults() {
    let isolated_home = TempDir::new().expect("failed to create isolated home directory");

    let output = Command::new(env!("CARGO_BIN_EXE_base"))
        .args(["snapshot", "download", "--help"])
        .env("NO_COLOR", "1")
        .env("HOME", isolated_home.path())
        .env("XDG_CONFIG_HOME", isolated_home.path())
        .env_remove("BASE_CHAIN")
        .output()
        .expect("failed to run base snapshot download --help");

    assert!(
        output.status.success(),
        "snapshot download help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let help = String::from_utf8(output.stdout).expect("snapshot download help must be UTF-8");
    let help = help.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        "Browse available snapshots at https://chain.base.org",
        "https://mainnet-v2-snapshots.base.org (--chain mainnet)",
        "https://sepolia-v2-snapshots.base.org (--chain sepolia)",
        "https://zeronet-v2-snapshots.base.org (--chain zeronet)",
    ] {
        assert!(help.contains(expected), "snapshot download help is missing {expected:?}");
    }
    for legacy_source in ["snapshots.reth.rs", "publicnode.com/snapshots"] {
        assert!(
            !help.contains(legacy_source),
            "snapshot download help still advertises legacy source {legacy_source:?}"
        );
    }
}
