use std::process::Command;

use sp1_build::{build_program_with_args, BuildArgs};

/// Build a native program.
fn build_native_program(program: &str) {
    let status = Command::new("cargo")
        .args(["build", "--workspace", "--bin", program, "--profile", "release-client-lto"])
        .status()
        .expect("Failed to execute cargo build command");

    if !status.success() {
        panic!("Failed to build {}", program);
    }

    println!("cargo:warning={} built with release-client-lto profile", program);
}

/// Build a program for the zkVM.
fn build_zkvm_program(program: &str) {
    build_program_with_args(
        &format!("../client-programs/{}", program),
        BuildArgs { elf_name: format!("{}-elf", program), docker: true, ..Default::default() },
    );
}

fn main() {
    let programs = vec!["fault-proof", "range"];
    for program in programs {
        build_native_program(program);
        build_zkvm_program(program);
    }

    build_zkvm_program("aggregation");
}
