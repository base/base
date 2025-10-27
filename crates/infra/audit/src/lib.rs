pub mod archiver;
pub mod publisher;
pub mod reader;
pub mod storage;
pub mod types;

use tokio::sync::mpsc;
use tracing::error;

pub use archiver::*;
pub use publisher::*;
pub use reader::*;
pub use storage::*;
pub use types::*;

pub fn connect_audit_to_publisher<P>(event_rx: mpsc::UnboundedReceiver<BundleEvent>, publisher: P)
where
    P: BundleEventPublisher + 'static,
{
    tokio::spawn(async move {
        let mut event_rx = event_rx;
        while let Some(event) = event_rx.recv().await {
            if let Err(e) = publisher.publish(event).await {
                error!(error = %e, "Failed to publish bundle event");
            }
        }
    });
}
