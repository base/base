use std::{sync::Arc, time::Duration};

use base_replay::{ReplayRecord, ReplayStream};
use base_tx_forwarding::{
    ForwardRequest, ForwardingSetupError, TxForwardingConfig, TxForwardingService,
};
use eyre::{Result, ensure};
use jsonrpsee::core::ClientError;
use reth_tasks::TaskExecutor;
use tokio::{sync::mpsc, task::AbortHandle};
use url::Url;

use crate::Injector;

/// A captured call with final delivery feedback from the shared forwarder.
#[derive(Debug)]
pub struct ReplayRequest {
    /// Original method and raw parameters.
    pub record: ReplayRecord,
    /// Final transport or RPC outcome.
    pub result: mpsc::Sender<Result<(), Arc<ClientError>>>,
}

impl ForwardRequest for ReplayRequest {
    fn method(&self) -> &str {
        &self.record.method
    }
    fn raw_params(&self) -> Option<&serde_json::value::RawValue> {
        Some(&self.record.params)
    }
    fn tx_hash(&self) -> Option<alloy_primitives::TxHash> {
        self.record.tx_hash
    }
    fn on_result(&self, result: Result<(), Arc<ClientError>>) {
        let _ = self.result.try_send(result);
    }
}

/// Pass-through builder ingress. Dropping aborts outstanding forwarding tasks.
#[derive(Debug)]
pub struct BuilderIngressInjector {
    sender: mpsc::Sender<ReplayRequest>,
    aborts: Vec<AbortHandle>,
}

impl BuilderIngressInjector {
    /// Starts a bounded, unthrottled single-call queue; returns endpoint setup errors.
    pub fn new(url: Url, executor: &TaskExecutor) -> Result<Self, ForwardingSetupError> {
        let (sender, receiver) = mpsc::channel(1);
        let forwarding = TxForwardingService::new(
            TxForwardingConfig::new(vec![url.clone()]).with_max_rps(0).with_max_batch_size(1),
        )
        .spawn_requests(vec![(url, receiver)], executor)?;
        let aborts = forwarding.abort_handles();
        Ok(Self { sender, aborts })
    }
}

impl Injector for BuilderIngressInjector {
    fn stream(&self) -> ReplayStream {
        ReplayStream::BuilderIngress
    }

    async fn inject(&self, record: ReplayRecord) -> Result<()> {
        ensure!(record.stream == self.stream(), "record stream does not match injector");
        tracing::debug!(method = %record.method, received_at_ms = record.received_at_ms, "injecting replay call");
        let (result, mut received) = mpsc::channel(1);
        tokio::time::timeout(Duration::from_secs(15), async {
            self.sender.send(ReplayRequest { record, result }).await?;
            received
                .recv()
                .await
                .ok_or_else(|| eyre::eyre!("forwarder closed without delivery feedback"))??;
            Ok(())
        })
        .await?
    }
}

impl Drop for BuilderIngressInjector {
    fn drop(&mut self) {
        for task in &self.aborts {
            task.abort();
        }
    }
}
