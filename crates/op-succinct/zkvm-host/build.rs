use std::process::Command;

use sp1_helper::{build_program_with_args, BuildArgs};

fn main() {
    // Build the zkvm program with the release-client-lto profile for native execution.
    let status = Command::new("cargo")
        .args([
            "build",
            "--workspace",
            "--bin",
            "zkvm-client",
            "--profile",
            "release-client-lto",
        ])
        .status()
        .expect("Failed to execute cargo build command");

    if !status.success() {
        panic!("Failed to build zkvm-client with release-client-lto profile");
    }

    println!("cargo:warning=zkvm-client built with release-client-lto profile");

    build_program_with_args(
        "../zkvm-client",
        BuildArgs {
            ignore_rust_version: true,
            ..Default::default()
        },
    );
}
