//! Test utilities for metering integration tests.

use std::sync::Arc;

use base_client_node::test_utils::{create_provider_factory, load_chain_spec};
use eyre::Context;
use reth::api::NodeTypesWithDBAdapter;
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::OpNode;
use reth_primitives_traits::SealedHeader;
use reth_provider::{HeaderProvider, providers::BlockchainProvider};

/// Test context providing the arguments needed by metering functions.
///
/// This bundles the provider, header, and chain spec that `meter_bundle()` and
/// `meter_block()` require. Use this for testing metering logic directly without
/// spinning up a full node.
///
/// For RPC integration tests, use [`TestHarness`] with [`MeteringExtension`]
/// instead:
///
/// ```ignore
/// use base_client_node::test_utils::TestHarness;
/// use base_metering::MeteringExtension;
///
/// let harness = TestHarness::builder()
///     .with_ext::<MeteringExtension>(true)
///     .build()
///     .await?;
/// ```
///
/// # Example
///
/// ```ignore
/// let ctx = MeteringTestContext::new()?;
/// let state = ctx.provider.state_by_block_hash(ctx.header.hash())?;
/// let result = meter_bundle(state, ctx.chain_spec.clone(), bundle, &ctx.header)?;
/// ```
#[derive(Debug, Clone)]
pub struct MeteringTestContext {
    /// Blockchain provider for state access.
    pub provider:
        BlockchainProvider<NodeTypesWithDBAdapter<OpNode, Arc<TempDatabase<DatabaseEnv>>>>,
    /// Genesis header - used as parent block for metering simulations.
    pub header: SealedHeader,
    /// Chain specification for EVM configuration.
    pub chain_spec: Arc<OpChainSpec>,
}

impl MeteringTestContext {
    /// Creates a new test context with genesis state initialized.
    pub fn new() -> eyre::Result<Self> {
        let chain_spec = load_chain_spec();

        let factory = create_provider_factory::<OpNode>(chain_spec.clone());
        reth_db_common::init::init_genesis(&factory).context("initializing genesis state")?;

        let provider = BlockchainProvider::new(factory).context("creating provider")?;
        let header = provider
            .sealed_header(0)
            .context("fetching genesis header")?
            .expect("genesis header exists");

        Ok(Self { provider, header, chain_spec })
    }
}
