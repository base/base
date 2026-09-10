//! Test utilities for the Base block builder.

mod apis;
mod driver;
mod external;
mod external_engine;
pub use crate::test_utils::EngineApi;
pub use external_engine::ExternalEngineApi;
mod instance;
mod txs;
mod utils;

use alloy_primitives::B256;
pub use apis::*;
use base_common_client_ethereum::{PrivateKeySigner, TxSignerSync};
use base_common_types_chain::Recovered;
use base_common_types_chain::{BaseTransactionSigned, BaseTypedTransaction};
use base_node_service::NodeConfig;
pub use driver::*;
pub use external::*;
pub use instance::*;
use k256::sha2::{Digest, Sha256};
pub use txs::*;
pub use utils::*;

use base_node_service::BuilderConfig;

/// Signs a Base transaction and returns the recovered signed transaction.
pub fn sign_base_tx(
    signer: &PrivateKeySigner,
    mut tx: BaseTypedTransaction,
) -> eyre::Result<Recovered<BaseTransactionSigned>> {
    let signature = signer
        .sign_transaction_sync(&mut tx)
        .map_err(|e| eyre::eyre!("failed to sign transaction: {e}"))?;
    let signed = BaseTransactionSigned::new_unhashed(tx, signature);
    Ok(Recovered::new_unchecked(signed, signer.address()))
}

/// Generates a signer deterministically from a seed (for testing only).
pub fn generate_signer_from_seed(seed: &str) -> PrivateKeySigner {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let hash = hasher.finalize();
    PrivateKeySigner::from_bytes(&B256::from_slice(&hash))
        .expect("Failed to create signer from seed")
}

/// Sets up a test instance with default builder configuration.
/// This is the simplified replacement for the `rb_test` macro.
pub async fn setup_test_instance() -> eyre::Result<LocalInstance> {
    clear_otel_env_vars();
    LocalInstance::new(BuilderConfig::for_tests()).await
}

/// Sets up a test instance with custom `BuilderConfig`.
pub async fn setup_test_instance_with_builder_config(
    config: BuilderConfig,
) -> eyre::Result<LocalInstance> {
    clear_otel_env_vars();
    LocalInstance::new(config).await
}

/// Sets up a test instance with custom `BuilderConfig` and `NodeConfig`.
pub async fn setup_test_instance_with_node_config(
    builder_config: BuilderConfig,
    node_config: NodeConfig,
) -> eyre::Result<LocalInstance> {
    clear_otel_env_vars();
    LocalInstance::new_with_node_config(builder_config, node_config).await
}

/// Hardcoded builder private key (anvil default key[1]).
pub const BUILDER_PRIVATE_KEY: &str =
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
/// Hardcoded funded account private key (anvil default key[0]).
pub const FUNDED_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// Default block gas limit used in tests.
pub const DEFAULT_GAS_LIMIT: u64 = 10_000_000;

/// Default EIP-1559 base fee denominator used in tests.
pub const DEFAULT_DENOMINATOR: u32 = 50;

/// Default EIP-1559 elasticity multiplier used in tests.
pub const DEFAULT_ELASTICITY: u32 = 2;
/// Default JWT secret token for authenticating Engine API requests in tests.
pub const DEFAULT_JWT_TOKEN: &str =
    "688f5d737bad920bdfb2fc2f488d6b6209eebda1dae949a8de91398d932c517a";

/// One ETH expressed in wei (10^18).
pub const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// This gets invoked before any tests, when the cargo test framework loads the test library.
/// It injects itself into
#[ctor::ctor]
fn init_tests() {
    // Clear OTEL env vars that may interfere with CLI argument parsing
    clear_otel_env_vars();

    use tracing_subscriber::{filter::filter_fn, layer::SubscriberExt, util::SubscriberInitExt};
    if let Ok(v) = std::env::var("TEST_TRACE") {
        let level = match v.as_str() {
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
