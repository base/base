//! Binary that fetches all trie state touched by a target L2 block from an RPC endpoint
//! and writes a self-contained regression fixture to disk for stateless block replay.

use std::path::PathBuf;

use clap::Parser;
use tracing::info;

/// Fetches all state touched by a target block from an L2 RPC endpoint and writes a
/// self-contained regression fixture (RocksDB trie-node store + `fixture.json`) to disk.
///
/// The fixture can be loaded by [`run_test_fixture`] to replay the block statelessly.
/// When the fixture was created for a block that exhibited a state-root bug the regression
/// test will fail (our execution produces a different hash) until the underlying issue is
/// fixed, at which point it passes.
///
/// # Example
/// ```text
/// cargo run -p fetch-regression-fixture -- \
///     --rpc-url https://base-mainnet.g.alchemy.com/v2/<key> \
///     --block 43127820 \
///     --out /tmp/fixtures
/// ```
#[derive(Debug, Parser)]
struct Args {
    /// HTTP(S) URL of the L2 execution-layer RPC endpoint.
    ///
    /// The node must expose the `debug_dbGet` and `debug_getRawTransaction` geth debug
    /// methods in addition to the standard `eth_` namespace.
    #[arg(long, env = "L2_RPC_URL")]
    rpc_url: String,

    /// L2 block number to create the regression fixture for.
    #[arg(long, default_value = "43127820")]
    block: u64,

    /// Directory under which `block-<N>.tar.gz` is written.
    #[arg(long, default_value = ".")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt().init();

    let args = Args::parse();

    info!(
        block = args.block,
        out = %args.out.display(),
        "Creating regression fixture",
    );

    let creator = base_proof_executor::test_utils::ExecutorTestFixtureCreator::new(
        &args.rpc_url,
        args.block,
        args.out.clone(),
    );

    creator.create_regression_fixture().await;

    let fixture_path = args.out.join(format!("block-{}.tar.gz", args.block));
    info!(path = %fixture_path.display(), "Fixture written successfully");

    Ok(())
}
