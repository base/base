use base_reth_test_utils::harness::TestHarness;
use eyre::Result;
use reth::api::FullNodeComponents;
use reth_exex::ExExContext;

use futures_util::{Future, TryStreamExt};
use reth_exex::{ExExEvent, ExExNotification};
use reth_tracing::tracing::info;

#[cfg(test)]
#[tokio::test]
async fn test_framework_test() -> Result<()> {
    use base_reth_test_utils::node::default_launcher;

    let harness = TestHarness::new(default_launcher).await?;
    let provider = harness.provider();

    Ok(())
}
