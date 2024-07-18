use sp1_helper::{build_program_with_args, BuildArgs};

fn main() {
    build_program_with_args(
        "../zkvm-client",
        BuildArgs {
            ignore_rust_version: true,
            ..Default::default()
        },
    );
}
