//! Test utilities for metering integration tests.

use base_reth_test_utils::{OpAddOns, OpBuilder};
use reth::builder::NodeHandle;
use reth_e2e_test_utils::Adapter;
use reth_optimism_node::OpNode;

use crate::{MeteringApiImpl, MeteringApiServer};

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
