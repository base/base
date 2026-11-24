//! `base-reth-node` with the `fuzz_coverage` RPC method (see the README).

use base_diff_node::CoverageRpcExtension;
use base_execution_cli::{Cli, StandardBaseRethNode, StandardNodeArgs};

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    base_cli_utils::init_common!();
    base_reth_cli::init_reth!();
    base_reth_cli::init_snapshots!();
    base_reth_cli::init_engine!();

    let cli = base_cli_utils::parse_cli!(Cli<StandardNodeArgs>);

    cli.run(|builder, args| async move {
        let builder = StandardBaseRethNode::apply_initial_upgrade_signal(builder, &args).await?;
        let mut runner = StandardBaseRethNode::runner_with_version_metrics(args)?;
        runner.install_ext::<CoverageRpcExtension>(());
        runner.run(builder).await
    })
    .unwrap();
}
