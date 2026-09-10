//! Built-in proof history processes canonical blocks with either storage backend.

use std::{sync::Arc, time::Duration};

use base_common_chain_config::BaseChainSpecBuilder;
use base_common_runtime_tasks::Runtime;
use base_execution_state_provider::test_utils::create_test_provider_factory_with_chain_spec;
use base_execution_state_tasks::InitializationJob;
use base_node_service::{
    BaseNode, NodeConfig, ProofHistory, ProofHistoryBackend, ProofsHistoryDbBackend, RollupArgs,
};
use base_testing_devnet::{
    BaseNodeTestUtils, node::NodeTestContext, transaction::TransactionTestContext, wallet::Wallet,
};

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_tracks_canonical_blocks_in_both_backends() -> eyre::Result<()> {
    for backend in [ProofsHistoryDbBackend::Mdbx, ProofsHistoryDbBackend::Rocksdb] {
        let storage_dir = tempfile::tempdir()?;
        let chain = Arc::new(
            BaseChainSpecBuilder::base_mainnet()
                .genesis(BaseNodeTestUtils::genesis())
                .ecotone_activated()
                .build(),
        );
        let wallet = Wallet::default().with_chain_id(chain.chain().into());
        let factory = create_test_provider_factory_with_chain_spec(chain.clone());
        base_execution_state_maintenance::init::init_genesis(&factory)?;
        let genesis_hash = chain.genesis_hash();
        let mut config = NodeConfig::new(chain).with_unused_ports();
        config.network.discovery.disable_discovery = true;
        let args = RollupArgs {
            proofs_history: true,
            proofs_history_storage_path: Some(storage_dir.path().join("proofs")),
            proofs_history_db: backend,
            ..Default::default()
        };
        let proofs = ProofHistory::open(&args)?;
        match proofs.backend {
            ProofHistoryBackend::Mdbx(storage) => {
                InitializationJob::new(storage, factory.provider()?.into_tx())
                    .run(0, genesis_hash)?
            }
            ProofHistoryBackend::Rocksdb(storage) => {
                InitializationJob::new(storage, factory.provider()?.into_tx())
                    .run(0, genesis_hash)?
            }
        }
        let runtime = Runtime::test();
        let mut launch = base_node_service::NodeLaunch::testing(config, runtime.clone());
        launch.base = BaseNode::new(args);
        let handle = launch.launch().await?;
        let progress = handle
            .node
            .proofs_progress
            .get()
            .expect("proof progress must be ready at launch")
            .clone();
        let mut node =
            NodeTestContext::new(handle.node, BaseNodeTestUtils::payload_attributes).await?;
        node.advance(1, |_| {
            Box::pin(TransactionTestContext::optimism_l1_block_info_tx(
                wallet.chain_id,
                wallet.inner.clone(),
                0,
            ))
        })
        .await?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if progress.latest().await?.is_some_and(|height| height >= 1) {
                    return Ok::<_, eyre::Report>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
    }
    Ok(())
}
