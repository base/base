use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base_consensus_node::{L1OriginSelector, OriginSelector};
use base_protocol::{BlockInfo, L2BlockInfo};

use crate::SharedL1Chain;

/// L1 origin selector adapter that supports test-controlled origin pinning.
#[derive(Debug)]
pub struct ActionOriginSelector {
    inner: L1OriginSelector<SharedL1Chain>,
    pin: Arc<Mutex<Option<BlockInfo>>>,
}

impl ActionOriginSelector {
    /// Create a new origin selector adapter.
    pub const fn new(
        inner: L1OriginSelector<SharedL1Chain>,
        pin: Arc<Mutex<Option<BlockInfo>>>,
    ) -> Self {
        Self { inner, pin }
    }
}

#[async_trait]
impl OriginSelector for ActionOriginSelector {
    async fn next_l1_origin(
        &mut self,
        unsafe_head: L2BlockInfo,
        is_recovery_mode: bool,
    ) -> Result<BlockInfo, base_consensus_node::L1OriginSelectorError> {
        if let Some(pin) = *self.pin.lock().expect("L1 origin pin lock poisoned") {
            return Ok(pin);
        }
        self.inner.next_l1_origin(unsafe_head, is_recovery_mode).await
    }
}
