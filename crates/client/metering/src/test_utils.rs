//! Test utilities for metering integration tests.

use std::sync::Arc;

use base_reth_test_utils::{OpAddOns, OpBuilder, create_provider_factory, load_genesis};
use eyre::Context;
use reth::api::NodeTypesWithDBAdapter;
use reth::builder::NodeHandle;
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_e2e_test_utils::Adapter;
use reth_optimism_chainspec::{OpChainSpec, OpChainSpecBuilder};
use reth_optimism_node::OpNode;
use reth_primitives_traits::SealedHeader;
use reth_provider::{HeaderProvider, providers::BlockchainProvider};

use crate::{MeteringApiImpl, MeteringApiServer};

/// Test context providing the arguments needed by metering functions.
///
/// This bundles the provider, header, and chain spec that `meter_bundle()` and
/// `meter_block()` require. Use this for testing metering logic directly without
/// spinning up a full node.
///
/// For RPC integration tests, use [`TestHarness::with_launcher(metering_launcher)`]
/// instead.
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
    pub provider: BlockchainProvider<NodeTypesWithDBAdapter<OpNode, Arc<TempDatabase<DatabaseEnv>>>>,
    /// Genesis header - used as parent block for metering simulations.
    pub header: SealedHeader,
    /// Chain specification for EVM configuration.
    pub chain_spec: Arc<OpChainSpec>,
}

impl MeteringTestContext {
    /// Creates a new test context with genesis state initialized.
    pub fn new() -> eyre::Result<Self> {
        let genesis = load_genesis();
        let chain_spec = Arc::new(
            OpChainSpecBuilder::base_mainnet()
                .genesis(genesis)
                .isthmus_activated()
                .build(),
        );

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

/// Node launcher that adds the metering RPC module.
///
/// Use with [`TestHarness::with_launcher`] for RPC integration tests:
///
/// ```ignore
/// let harness = TestHarness::with_launcher(metering_launcher).await?;
/// let client = RpcClient::new_http(harness.rpc_url().parse()?);
/// let response: MeterBundleResponse = client.request("base_meterBundle", (bundle,)).await?;
/// ```
pub async fn metering_launcher(
    builder: OpBuilder,
) -> eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>> {
    let launcher = builder.engine_api_launcher();
    builder
        .extend_rpc_modules(|ctx| {
            let metering_api = MeteringApiImpl::new(ctx.provider().clone());
            ctx.modules.merge_configured(metering_api.into_rpc())?;
            Ok(())
        })
        .launch_with(launcher)
        .await
}
