//! Staged syncing primitives for reth.
//!
//! This crate contains the syncing primitives [`Pipeline`] and [`Stage`], as well as all stages
//! that reth uses to sync.
//!
//! A pipeline can be configured using [`Pipeline::builder()`].
//!
//! For ease of use, this crate also exposes a set of [`StageSet`]s, which are collections of stages
//! that perform specific functions during sync. Stage sets can be customized; it is possible to
//! add, disable and replace stages in the set.
//!
//! # Examples
//!
//! ```
//! # use std::sync::Arc;
//! # use reth_downloaders::bodies::bodies::BodiesDownloaderBuilder;
//! # use reth_downloaders::headers::reverse_headers::ReverseHeadersDownloaderBuilder;
//! # use reth_network_p2p::test_utils::{TestBodiesClient, TestHeadersClient};
//! # use alloy_primitives::B256;
//! #
//! # use base_execution_state_types::PruneModes;
//! # use base_execution_network_types::PeerId;
//! # use reth_stages::Pipeline;
//! # use reth_stages::sets::DefaultStages;
//! # use tokio::sync::watch;
//! # use base_execution_evm_blocks::BaseEvmConfig;
//! # use base_execution_state_provider::ProviderFactory;
//! # use base_execution_state_provider::StaticFileProviderFactory;
//! # use base_execution_state_provider::test_utils::{create_test_provider_factory, MockNodeDatabase};
//! # use base_execution_state_maintenance::StaticFileProducer;
//! # use reth_config::config::StageConfig;
//! # use base_execution_evm_blocks::Consensus;
//! # use base_execution_evm_blocks::BaseBeaconConsensus;
//! # use base_execution_evm_blocks::BaseBeaconConsensus;
//! #
//! # let chain_spec = std::sync::Arc::new(base_common_chain_config::BaseChainSpec::mainnet());
//! # let consensus: Arc<BaseBeaconConsensus> = Arc::new(TestConsensus::test());
//! # let headers_downloader = ReverseHeadersDownloaderBuilder::default().build(
//! #    Arc::new(TestHeadersClient::default()),
//! #    consensus.clone()
//! # );
//! # let provider_factory = create_test_provider_factory();
//! # let bodies_downloader = BodiesDownloaderBuilder::default().build(
//! #    Arc::new(TestBodiesClient { responder: |_| Ok((PeerId::ZERO, vec![]).into()) }),
//! #    consensus.clone(),
//! #    provider_factory.clone()
//! # );
//! # let (tip_tx, tip_rx) = watch::channel(B256::default());
//! # let executor_provider = BaseEvmConfig::default();
//! # let static_file_producer = StaticFileProducer::new(
//! #    provider_factory.clone(),
//! #    PruneModes::default()
//! # );
//! // Create a pipeline that can fully sync
//! # let pipeline =
//! Pipeline::<MockNodeDatabase>::builder()
//!     .with_tip_sender(tip_tx)
//!     .add_stages(DefaultStages::new(
//!         provider_factory.clone(),
//!         tip_rx,
//!         consensus,
//!         headers_downloader,
//!         bodies_downloader,
//!         executor_provider,
//!         StageConfig::default(),
//!         PruneModes::default(),
//!     ))
//!     .build(provider_factory, static_file_producer);
//! ```
//!
//! ## Feature Flags
//!
//! - `test-utils`: Export utilities for testing

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[expect(missing_docs)]
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

/// A re-export of common structs and traits.
pub mod prelude;

/// Implementations of stages.
pub mod stages;

pub mod sets;

// re-export the stages API
pub use reth_stages_api::*;
