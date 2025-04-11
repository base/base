use sp1_build::{build_program_with_args, BuildArgs};

#[allow(unused)]
fn build_program(program_name: &str, elf_name: &str, features: Option<Vec<String>>) {
    let metadata =
        cargo_metadata::MetadataCommand::new().exec().expect("Failed to get cargo metadata");

    let mut build_args = BuildArgs {
        elf_name: Some(elf_name.to_string()),
        output_directory: Some("../../elf".to_string()),
        docker: true,
        tag: "v4.1.7".to_string(),
        workspace_directory: Some("../../".to_string()),
        ..Default::default()
    };

    if let Some(features) = features {
        build_args.features = features;
    }

    build_program_with_args(
        &format!("{}/{}", metadata.workspace_root.join("programs"), program_name),
        build_args,
    );
}

/// Build all the native programs and the native host runner. Optional flag to build the zkVM
/// programs.
pub fn build_all() {
    // build_program("aggregation", "aggregation-elf", None);
    // build_program("range", "range-elf-bump", None);
    // build_program("range", "range-elf-embedded", Some(vec!["embedded".to_string()]));
    // build_program(
    //     "range",
    //     "celestia-range-elf-embedded",
    //     Some(vec!["embedded".to_string(), "celestia".to_string()]),
    // );
}
