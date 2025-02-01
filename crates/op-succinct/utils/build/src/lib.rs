use sp1_build::{build_program_with_args, BuildArgs};

#[derive(Clone, Copy)]
pub enum ProgramBuildArgs {
    Default,
    WithTracing,
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
            elf_name: Some(format!("{}-elf", program)),
            output_directory: Some("../../elf".to_string()),
            docker: true,
            tag: "v4.0.0-rc.10".to_string(),
            workspace_directory: Some("../../".to_string()),
            ..Default::default()
        },
    );
}

/// Build all the native programs and the native host runner. Optional flag to build the zkVM
/// programs.
pub fn build_all() {
    // Build range program.
    // build_zkvm_program("range");

    // Build aggregation program.
    // build_zkvm_program("aggregation");
}
