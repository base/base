# Execution synchronization pipeline

Staged syncing primitives for reth.

This crate contains the syncing primitives [`Pipeline`] and [`Stage`], as well as all stages
that reth uses to sync.

A pipeline can be configured using [`Pipeline::builder()`].

For ease of use, this crate also exposes a set of [`StageSet`]s, which are collections of stages
that perform specific functions during sync. Stage sets can be customized; it is possible to
add, disable and replace stages in the set.

# Examples

```
# use std::sync::Arc;
# use base_execution_sync::BodiesDownloaderBuilder;
# use base_execution_sync::ReverseHeadersDownloaderBuilder;
# use base_execution_network_service::test_utils::{TestBodiesClient, TestHeadersClient};
# use alloy_primitives::B256;
#
# use base_execution_state_types::PruneModes;
# use base_execution_network_types::PeerId;
# use base_execution_sync::Pipeline;
# use base_execution_sync::DefaultStages;
# use tokio::sync::watch;
# use base_execution_evm_blocks::BaseEvmConfig;
# use base_execution_state_provider::ProviderFactory;
# use base_execution_state_provider::StaticFileProviderFactory;
# use base_execution_state_provider::test_utils::create_test_provider_factory;
# use base_execution_state_maintenance::StaticFileProducer;
# use base_execution_sync::StageConfig;
# use base_execution_evm_blocks::BaseBeaconConsensus;
#
# let chain_spec = std::sync::Arc::new(base_common_chain_config::BaseChainSpec::mainnet());
# let consensus: Arc<BaseBeaconConsensus> = Arc::new(BaseBeaconConsensus::new(chain_spec));
# let headers_downloader = ReverseHeadersDownloaderBuilder::default().build(
#    Arc::new(TestHeadersClient::default()),
#    consensus.clone()
# );
# let provider_factory = create_test_provider_factory();
# let bodies_downloader = BodiesDownloaderBuilder::default().build(
#    Arc::new(TestBodiesClient { responder: |_| Ok((PeerId::ZERO, vec![]).into()) }),
#    consensus.clone(),
#    provider_factory.clone()
# );
# let (tip_tx, tip_rx) = watch::channel(B256::default());
# let executor_provider = BaseEvmConfig::default();
# let static_file_producer = StaticFileProducer::new(
#    provider_factory.clone(),
#    PruneModes::default()
# );
// Create a pipeline that can fully sync
# let pipeline =
Pipeline::builder()
    .with_tip_sender(tip_tx)
    .add_stages(DefaultStages::new(
        provider_factory.clone(),
        tip_rx,
        consensus,
        headers_downloader,
        bodies_downloader,
        executor_provider,
        StageConfig::default(),
        PruneModes::default(),
    ))
    .build(provider_factory, static_file_producer);
```

## Feature Flags

- `test-utils`: Export utilities for testing


The file-backed pipeline integration suites use `file-client`. Run the complete suite with
`cargo test -p base-execution-sync --features file-client -- --test-threads=4`.
