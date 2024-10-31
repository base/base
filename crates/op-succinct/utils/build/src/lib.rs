use std::process::Command;

use sp1_build::{build_program_with_args, BuildArgs};

/// Build a native program.
fn build_native_program(program: &str) {
    // Build with tracing-subscriber feature enabled for `kona-client` logging.
    let status = Command::new("cargo")
        .args([
            "build",
            "--workspace",
            "--bin",
            program,
            "--profile",
            "release-client-lto",
            "--features",
            "tracing-subscriber",
        ])
        .status()
        .expect("Failed to execute cargo build command");

    if !status.success() {
        panic!("Failed to build {}", program);
    }

    println!(
        "cargo:warning={} built with release-client-lto profile",
        program
    );
}

/// Build the native host runner to a separate target directory to avoid build lockups.
fn build_native_host_runner() {
    let metadata = cargo_metadata::MetadataCommand::new()
        .exec()
        .expect("Failed to get cargo metadata");
    let target_dir = metadata.target_directory.join("native_host_runner");

    let status = Command::new("cargo")
        .args([
            "build",
            "--workspace",
            "--bin",
            "native_host_runner",
            "--release",
            "--target-dir",
            target_dir.as_ref(),
        ])
        .status()
        .expect("Failed to execute cargo build command");
    if !status.success() {
        panic!("Failed to build native_host_runner");
    }

    println!("cargo:warning=native_host_runner built with release profile",);
}

/// Build a program for the zkVM.
#[allow(dead_code)]
fn build_zkvm_program(program: &str) {
    let metadata = cargo_metadata::MetadataCommand::new()
        .exec()
        .expect("Failed to get cargo metadata");
    build_program_with_args(
        &format!("{}/{}", metadata.workspace_root.join("programs"), program),
        BuildArgs {
            elf_name: format!("{}-elf", program),
            docker: true,
            tag: "v3.0.0".to_string(),
            ..Default::default()
        },
    );
}

/// Build all the native programs and the native host runner. Optional flag to build the zkVM
/// programs.
pub fn build_all() {
    let programs = vec!["fault-proof", "range"];

    for program in programs {
        // Note: Don't comment this out, because the Docker program depends on the native program
        // for range being built.
        build_native_program(program);
        // build_zkvm_program(program);
    }

    // build_zkvm_program("aggregation");
    // Note: Don't comment this out, because the Docker program depends on the native host runner
    // being built.
    build_native_host_runner();
}
