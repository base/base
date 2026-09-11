//! Captures the short git SHA of the build so reports can name the exact commit.
//!
//! Falls back to `unknown` rather than failing, so source-tarball and Docker builds without
//! git metadata still compile.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BASE_TELEMETRY_GIT_SHA");

    if std::env::var_os("BASE_TELEMETRY_GIT_SHA").is_some() {
        return;
    }

    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=BASE_TELEMETRY_GIT_SHA={sha}");
}
