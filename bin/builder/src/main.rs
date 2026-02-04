#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use base_builder_cli::Cli;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    base_cli_utils::Backtracing::enable();
    base_cli_utils::SigsegvHandler::install();

    // Initialize Reth version metadata for P2P identification and logging.
    base_cli_utils::init_reth_version!();

    let cli = base_cli_utils::parse_cli!(Cli, |cmd: clap::Command| {
        cmd.mut_arg("log_file_directory", |arg: clap::Arg| {
            arg.default_value(base_cli_utils::logs_dir!())
        })
    });

    if let Err(err) = base_builder_core::launch(cli) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
