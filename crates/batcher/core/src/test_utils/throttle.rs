//! Test [`ThrottleClient`] implementations.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::ThrottleClient;

/// Shared handle to the call log of a [`TrackingThrottleClient`].
pub type ThrottleCallLog = Arc<Mutex<Vec<(u64, u64)>>>;

/// [`ThrottleClient`] that records every `set_max_da_size` call in order.
///
/// Use [`TrackingThrottleClient::new`] to obtain both the client and a shared
/// handle to the recorded calls.
#[derive(Debug, Default, Clone)]
pub struct TrackingThrottleClient {
    /// Recorded `(max_tx_size, max_block_size)` calls in order.
    pub calls: ThrottleCallLog,
}

impl TrackingThrottleClient {
    /// Create a new client and return a shared reference to the recorded calls.
    pub fn new() -> (Self, ThrottleCallLog) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (Self { calls: Arc::clone(&calls) }, calls)
    }
}

impl ThrottleClient for TrackingThrottleClient {
    fn set_max_da_size(
        &self,
        max_tx_size: u64,
        max_block_size: u64,
    ) -> BoxFuture<'_, Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls.lock().unwrap().push((max_tx_size, max_block_size));
            Ok(())
        })
    }
}
