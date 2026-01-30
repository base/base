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

    let logs_dir = dirs_next::cache_dir()
        .map(|root| root.join(format!("{}/logs", env!("CARGO_PKG_NAME"))))
        .unwrap()
        .into_os_string();

    let cli = base_cli_utils::parse_cli!(Cli, |cmd: base_cli_utils::clap::Command| {
        cmd.mut_arg("log_file_directory", |arg: base_cli_utils::clap::Arg| {
            arg.default_value(logs_dir)
        })
    });

    if let Err(err) = op_rbuilder::launcher::launch(cli) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
