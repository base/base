#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    base_cli_utils::Backtracing::enable();
    base_cli_utils::SigsegvHandler::install();

    if let Err(err) = op_rbuilder::launcher::launch() {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
