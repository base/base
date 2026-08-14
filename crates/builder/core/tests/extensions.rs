//! End-to-end tests for node extensions installed through [`LocalInstanceBuilder`].

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use base_builder_core::{BuilderConfig, test_utils::LocalInstanceBuilder};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};

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
