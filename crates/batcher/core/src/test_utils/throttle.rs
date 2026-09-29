//! Test [`ThrottleClient`] implementation.
//!
//! Hand-rolled rather than mocked because the integration tests under `tests/` use it, and an
//! `automock` mock is only built under `cfg(test)`, which they cannot see.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::ThrottleClient;

/// Shared handle to the call log of a [`TrackingThrottleClient`].
pub type ThrottleCallLog = Arc<Mutex<Vec<(u64, u64)>>>;

/// [`ThrottleClient`] that records every `set_max_da_size` call in order, failed ones included.
#[derive(Debug)]
pub struct TrackingThrottleClient {
    calls: ThrottleCallLog,
    failures: Mutex<usize>,
}

impl TrackingThrottleClient {
    /// Create a client whose calls all succeed, and its shared call log.
    pub fn new() -> (Self, ThrottleCallLog) {
        let calls = ThrottleCallLog::default();
        (Self { calls: Arc::clone(&calls), failures: Mutex::new(0) }, calls)
    }

    /// Make the first `failures` calls fail.
    pub fn with_failures(self, failures: usize) -> Self {
        Self { failures: Mutex::new(failures), ..self }
    }
}

impl ThrottleClient for TrackingThrottleClient {
    fn set_max_da_size(
        &self,
        max_tx_size: u64,
        max_block_size: u64,
    ) -> BoxFuture<'_, Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        self.calls.lock().unwrap().push((max_tx_size, max_block_size));
        let mut failures = self.failures.lock().unwrap();
        let result = if *failures > 0 {
            *failures -= 1;
            Err("the block builder refused the limits".into())
        } else {
            Ok(())
        };
        Box::pin(async move { result })
    }
}
