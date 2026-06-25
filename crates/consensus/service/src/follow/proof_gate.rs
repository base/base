use std::{fmt::Debug, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::time;

use crate::follow::{error::FollowError, local::FollowLocalClient};

const PROOF_GATE_RETRY_INTERVAL: Duration = Duration::from_millis(250);

#[async_trait]
pub(super) trait ProofGate: Debug + Send {
    async fn wait_til_ready(&mut self, current_block: u64) -> Result<(), FollowError>;
}

#[derive(Debug)]
pub(super) struct ActiveProofGate<Local> {
    local: Arc<Local>,
    max_blocks_ahead: u64,
    cap: u64,
}

impl<Local> ActiveProofGate<Local>
where
    Local: FollowLocalClient + 'static,
{
    pub(super) async fn new(local: Arc<Local>, max_blocks_ahead: u64) -> Result<Self, FollowError> {
        let mut gate = Self { local, max_blocks_ahead, cap: 0 };
        gate.refresh().await?;
        Ok(gate)
    }

    async fn refresh(&mut self) -> Result<(), FollowError> {
        let latest = self.local.proofs_latest().await?.unwrap_or(0);
        self.cap = latest.saturating_add(self.max_blocks_ahead);
        debug!(target: "follow", proofs_latest = latest, cap = self.cap, "Proof gate refreshed");
        Ok(())
    }
}

#[async_trait]
impl<Local> ProofGate for ActiveProofGate<Local>
where
    Local: FollowLocalClient + 'static,
{
    async fn wait_til_ready(&mut self, current_block: u64) -> Result<(), FollowError> {
        while current_block > self.cap {
            self.refresh().await?;
            if current_block > self.cap {
                time::sleep(PROOF_GATE_RETRY_INTERVAL).await;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub(super) struct NoopProofGate;

#[async_trait]
impl ProofGate for NoopProofGate {
    async fn wait_til_ready(&mut self, _current_block: u64) -> Result<(), FollowError> {
        Ok(())
    }
}
