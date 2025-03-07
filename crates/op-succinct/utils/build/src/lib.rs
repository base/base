#[allow(unused)]
use sp1_build::{build_program_with_args, BuildArgs};

/// Build all the native programs and the native host runner. Optional flag to build the zkVM
/// programs.
pub fn build_all() {
    // let metadata = cargo_metadata::MetadataCommand::new()
    //     .exec()
    //     .expect("Failed to get cargo metadata");
    // build_program_with_args(
    //     &format!("{}/{}", metadata.workspace_root.join("programs"), "aggregation"),
    //     BuildArgs {
    //         elf_name: Some("aggregation-elf".to_string()),
    //         output_directory: Some("../../elf".to_string()),
    //         docker: true,
    //         tag: "v4.1.2".to_string(),
    //         workspace_directory: Some("../../".to_string()),
    //         ..Default::default()
    //     },
    // );

    // build_program_with_args(
    //     &format!("{}/{}", metadata.workspace_root.join("programs"), "range"),
    //     BuildArgs {
    //         elf_name: Some("range-elf-bump".to_string()),
    //         output_directory: Some("../../elf".to_string()),
    //         docker: true,
    //         tag: "v4.1.2".to_string(),
    //         workspace_directory: Some("../../".to_string()),
    //         ..Default::default()
    //     },
    // );

    // build_program_with_args(
    //     &format!("{}/{}", metadata.workspace_root.join("programs"), "range"),
    //     BuildArgs {
    //         elf_name: Some("range-elf-embedded".to_string()),
    //         output_directory: Some("../../elf".to_string()),
    //         docker: true,
    //         tag: "v4.1.2".to_string(),
    //         workspace_directory: Some("../../".to_string()),
    //         features: vec!["embedded".to_string()],
    //         ..Default::default()
    //     },
    // );
}
