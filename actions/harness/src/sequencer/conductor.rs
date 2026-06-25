use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_node::{Conductor, ConductorError};

/// Conductor adapter that allows the actor to own a cloneable conductor handle.
#[derive(Debug, Clone)]
pub struct ActionConductor {
    inner: Arc<Mutex<Option<Arc<dyn Conductor>>>>,
}

impl ActionConductor {
    /// Create a new conductor adapter.
    pub fn new(inner: Arc<Mutex<Option<Arc<dyn Conductor>>>>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Conductor for ActionConductor {
    async fn leader(&self) -> Result<bool, ConductorError> {
        let conductor = self.inner.lock().expect("conductor lock poisoned").clone();
        match conductor {
            Some(conductor) => conductor.leader().await,
            None => Ok(true),
        }
    }

    async fn active(&self) -> Result<bool, ConductorError> {
        let conductor = self.inner.lock().expect("conductor lock poisoned").clone();
        match conductor {
            Some(conductor) => conductor.active().await,
            None => Ok(true),
        }
    }

    async fn commit_unsafe_payload(
        &self,
        payload: &BaseExecutionPayloadEnvelope,
    ) -> Result<(), ConductorError> {
        let conductor = self.inner.lock().expect("conductor lock poisoned").clone();
        match conductor {
            Some(conductor) => conductor.commit_unsafe_payload(payload).await,
            None => Ok(()),
        }
    }

    async fn override_leader(&self) -> Result<(), ConductorError> {
        let conductor = self.inner.lock().expect("conductor lock poisoned").clone();
        match conductor {
            Some(conductor) => conductor.override_leader().await,
            None => Ok(()),
        }
    }
}
