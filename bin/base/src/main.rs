#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cli;
mod commands;
mod config;

#[cfg(all(feature = "mimalloc", feature = "jemalloc"))]
use rustfs_mimalloc as _;

// mimalloc is the default allocator. An explicitly enabled `jemalloc` feature (for example via
// `jemalloc-prof` profiling builds) takes precedence so heap profiling keeps working.
#[cfg(all(feature = "mimalloc", not(feature = "jemalloc")))]
#[global_allocator]
static ALLOC: rustfs_mimalloc::MiMalloc = rustfs_mimalloc::MiMalloc;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

#[cfg(all(feature = "jemalloc-prof", unix))]
#[unsafe(export_name = "malloc_conf")]
static MALLOC_CONF: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

fn main() {
    // Keep reth_info and network identity aligned with the Base binary version.
    base_reth_cli::init_reth!();
    base_reth_cli::init_snapshots!();

    base_cli_utils::run_cli_main!(cli::BaseCli);
}
