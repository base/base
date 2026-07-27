//! End-to-end tests for the [`LocalInstanceBuilder`] injection seams: a custom
//! [`CandidateSource`] wired into the flashblocks build loop, and a node
//! [`BaseNodeExtension`] applied through the same hook pipeline used in production.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use base_builder_core::{
    BoxedBestTransactions, BuilderConfig, CandidateSource, test_utils::LocalInstanceBuilder,
};
use base_execution_txpool::BasePooledTransaction;
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use reth_transaction_pool::BestTransactionsAttributes;

/// A candidate source that counts how many times the build loop drains it and otherwise passes the
/// pool's best-transactions stream through unchanged.
#[derive(Debug, Clone)]
struct CountingSource {
    calls: Arc<AtomicUsize>,
}

impl CandidateSource<BasePooledTransaction> for CountingSource {
    fn best_transactions(
        &self,
        pool_best: BoxedBestTransactions<BasePooledTransaction>,
        _attributes: BestTransactionsAttributes,
    ) -> BoxedBestTransactions<BasePooledTransaction> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        pool_best
    }
}

/// A minimal extension that flips a shared flag from its node-started hook, proving the extension's
/// `apply` runs through the same [`NodeHooks`] pipeline the production runner uses.
#[derive(Debug)]
struct StartedFlagExtension {
    started: Arc<AtomicBool>,
}

impl BaseNodeExtension for StartedFlagExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let started = self.started;
        hooks.add_node_started_hook(move |_full_node| {
            started.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

impl FromExtensionConfig for StartedFlagExtension {
    type Config = Arc<AtomicBool>;

    fn from_config(config: Self::Config) -> Self {
        Self { started: config }
    }
}

/// A custom [`CandidateSource`] supplied via [`LocalInstanceBuilder::with_candidate_source`] is
/// actually drained by the real flashblocks build loop when producing a block.
#[tokio::test]
async fn injected_candidate_source_is_used_by_the_build_loop() -> eyre::Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .with_candidate_source(CountingSource { calls: Arc::clone(&calls) })
        .build()
        .await?;

    let driver = instance.driver().await?;
    driver.build_new_block().await?;

    assert!(
        calls.load(Ordering::SeqCst) > 0,
        "the injected candidate source should be drained by the build loop"
    );
    Ok(())
}

/// An extension installed via [`LocalInstanceBuilder::install_ext`] has its hooks applied through
/// the production hook pipeline: its node-started hook runs once the node has started.
#[tokio::test]
async fn installed_extension_runs_through_the_hook_pipeline() -> eyre::Result<()> {
    let started = Arc::new(AtomicBool::new(false));
    let _instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<StartedFlagExtension>(Arc::clone(&started))
        .build()
        .await?;

    assert!(
        started.load(Ordering::SeqCst),
        "the installed extension's node-started hook should have run during launch"
    );
    Ok(())
}
