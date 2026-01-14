mod apis;
mod contracts;
mod driver;
mod external;
mod instance;
mod txs;
mod utils;

pub use apis::*;
pub use contracts::*;
pub use driver::*;
pub use external::*;
pub use instance::*;
use reth_node_builder::NodeConfig;
use reth_optimism_chainspec::OpChainSpec;
pub use txs::*;
pub use utils::*;

use crate::args::OpRbuilderArgs;

/// Sets up a test instance with default flashblocks configuration.
/// This is the simplified replacement for the rb_test macro.
pub async fn setup_test_instance() -> eyre::Result<LocalInstance> {
    clear_otel_env_vars();
    LocalInstance::flashblocks().await
}

/// Sets up a test instance with custom OpRbuilderArgs.
/// The flashblocks_port will be automatically set to an available port.
pub async fn setup_test_instance_with_args(
    mut args: OpRbuilderArgs,
) -> eyre::Result<LocalInstance> {
    clear_otel_env_vars();
    args.flashblocks.flashblocks_port = get_available_port();
    LocalInstance::new(args).await
}

/// Sets up a test instance with custom OpRbuilderArgs and NodeConfig.
/// The flashblocks_port will be automatically set to an available port.
pub async fn setup_test_instance_with_config(
    mut args: OpRbuilderArgs,
    config: NodeConfig<OpChainSpec>,
) -> eyre::Result<LocalInstance> {
    clear_otel_env_vars();
    args.flashblocks.flashblocks_port = get_available_port();
    LocalInstance::new_with_config(args, config).await
}

// anvil default key[1]
pub const BUILDER_PRIVATE_KEY: &str =
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
// anvil default key[0]
pub const FUNDED_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
// anvil default key[8]
pub const FLASHBLOCKS_DEPLOY_KEY: &str =
    "0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97";

pub const DEFAULT_GAS_LIMIT: u64 = 10_000_000;

pub const DEFAULT_DENOMINATOR: u32 = 50;

pub const DEFAULT_ELASTICITY: u32 = 2;
pub const DEFAULT_JWT_TOKEN: &str =
    "688f5d737bad920bdfb2fc2f488d6b6209eebda1dae949a8de91398d932c517a";

pub const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// This gets invoked before any tests, when the cargo test framework loads the test library.
/// It injects itself into
#[ctor::ctor]
fn init_tests() {
    // Clear OTEL env vars that may interfere with CLI argument parsing
    clear_otel_env_vars();

    use tracing_subscriber::{filter::filter_fn, prelude::*};
    if let Ok(v) = std::env::var("TEST_TRACE") {
        let level = match v.as_str() {
            "false" | "off" => return,
            "true" | "debug" | "on" => tracing::Level::DEBUG,
            "trace" => tracing::Level::TRACE,
            "info" => tracing::Level::INFO,
            "warn" => tracing::Level::WARN,
            "error" => tracing::Level::ERROR,
            _ => return,
        };

        // let prefix_blacklist = &["alloy_transport_ipc", "storage::db::mdbx"];
        let prefix_blacklist = &["storage::db::mdbx"];

        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .with(filter_fn(move |metadata| {
                metadata.level() <= &level
                    && !prefix_blacklist.iter().any(|prefix| metadata.target().starts_with(prefix))
            }))
            .init();
    }

    #[cfg(not(windows))]
    let _ = rlimit::setrlimit(rlimit::Resource::NOFILE, 500_000, 500_000);
}
