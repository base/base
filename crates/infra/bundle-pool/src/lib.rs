pub mod pool;
pub mod source;

use source::BundleSource;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::error;

pub use pool::{Action, BundleStore, InMemoryBundlePool, ProcessedBundle};
pub use source::KafkaBundleSource;
pub use tips_core::{AcceptedBundle, Bundle, CancelBundle};

pub fn connect_sources_to_pool<S, P>(
    sources: Vec<S>,
    bundle_rx: mpsc::UnboundedReceiver<AcceptedBundle>,
    pool: Arc<Mutex<P>>,
) where
    S: BundleSource + Send + 'static,
    P: BundleStore + Send + 'static,
{
    for source in sources {
        tokio::spawn(async move {
            if let Err(e) = source.run().await {
                error!(error = %e, "Bundle source failed");
            }
        });
    }

    tokio::spawn(async move {
        let mut bundle_rx = bundle_rx;
        while let Some(bundle) = bundle_rx.recv().await {
            pool.lock().unwrap().add_bundle(bundle);
        }
    });
}
