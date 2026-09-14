#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use base_execution_cli::{Cli, StandardBaseRethNode, StandardNodeArgs};

type NodeCli = Cli<StandardNodeArgs>;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

#[cfg(all(feature = "jemalloc-prof", unix))]
#[unsafe(export_name = "malloc_conf")]
static MALLOC_CONF: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

fn main() {
    base_cli_utils::init_common!();
    base_reth_cli::init_reth!();
    base_reth_cli::init_snapshots!();

    let cli = base_cli_utils::parse_cli!(NodeCli);

    cli.run(|builder, args| async move { StandardBaseRethNode::run(builder, args).await }).unwrap();
}
