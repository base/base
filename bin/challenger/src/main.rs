#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cli;

fn main() {
    let _ = reth_node_core::args::DefaultTraceValues::default()
        .with_service_name("base-challenger")
        .try_init();
    base_cli_utils::run_cli_main!(cli::Cli);
}
