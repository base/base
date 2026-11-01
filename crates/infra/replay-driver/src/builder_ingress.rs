use std::{sync::Arc, time::Duration};

use alloy_primitives::TxHash;
use base_replay::{ReplayRecord, ReplayStream};
use base_tx_forwarding::{
    ForwardRequest, ForwardingSetupError, TxForwardingConfig, TxForwardingService,
};
use eyre::{Result, ensure};
use jsonrpsee::{core::ClientError, types::error::INTERNAL_ERROR_CODE};
use reth_tasks::TaskExecutor;
use tokio::{sync::mpsc, task::AbortHandle};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::Injector;

/// A captured call with final delivery feedback from the shared forwarder.
#[derive(Debug)]
pub struct ReplayRequest {
    /// Original method and raw parameters.
    pub record: ReplayRecord,
    /// Final transport or RPC outcome.
    pub result: mpsc::Sender<Result<(), Arc<ClientError>>>,
    /// Cancelled when the delivering future completes, times out, or is dropped.
    pub cancellation: CancellationToken,
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
    fn cancellation(&self) -> Option<&CancellationToken> {
        Some(&self.cancellation)
    }
}

/// Pass-through builder ingress. Dropping aborts outstanding forwarding tasks.
#[derive(Debug)]
pub struct BuilderIngressInjector {
    sender: mpsc::Sender<ReplayRequest>,
    aborts: Vec<AbortHandle>,
}

impl BuilderIngressInjector {
    /// Recognizes only known pool obsolescence errors during restoration, not initial delivery.
    pub fn is_obsolete(error: &eyre::Report, hash: TxHash) -> bool {
        let Some(error) = error.downcast_ref::<Arc<ClientError>>() else { return false };
        let ClientError::Call(error) = error.as_ref() else { return false };
        let reason = match error.code() {
            INTERNAL_ERROR_CODE => {
                error.message().strip_prefix(&format!("pool rejected transaction: [{hash}]: "))
            }
            -32000 => Some(error.message()),
            _ => None,
        };
        let Some(reason) = reason else { return false };
        if matches!(
            reason,
            "already imported"
                | "already known"
                | "insufficient gas price to replace existing transaction"
                | "replacement transaction underpriced"
        ) {
            return true;
        }
        reason
            .strip_prefix("transaction nonce is not consistent: next nonce ")
            .or_else(|| reason.strip_prefix("nonce too low: next nonce "))
            .and_then(|nonces| nonces.split_once(", tx nonce "))
            .and_then(|(state, tx)| state.parse::<u64>().ok().zip(tx.parse::<u64>().ok()))
            .is_some_and(|(state, tx)| tx < state)
    }

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
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        tokio::time::timeout(Duration::from_secs(15), async {
            self.sender.send(ReplayRequest { record, result, cancellation }).await?;
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

#[cfg(test)]
mod tests {
    use jsonrpsee::types::ErrorObjectOwned;

    use super::*;

    #[test]
    fn skips_only_recognized_pool_obsolescence_during_restoration() {
        let hash = TxHash::ZERO;
        let pool_code = INTERNAL_ERROR_CODE;
        for (code, reason, expected) in [
            (pool_code, "already imported", true),
            (pool_code, "insufficient gas price to replace existing transaction", true),
            (pool_code, "transaction nonce is not consistent: next nonce 3, tx nonce 2", true),
            (pool_code, "transaction nonce is not consistent: next nonce 2, tx nonce 3", false),
            (pool_code, "transaction nonce is not consistent: next nonce x, tx nonce 2", false),
            (pool_code, "insufficient funds", false),
            (-32000, "already known", true),
            (-32000, "replacement transaction underpriced", true),
            (-32000, "nonce too low: next nonce 3, tx nonce 2", true),
            (-32000, "transaction underpriced", false),
            (-32602, "already known", false),
        ] {
            let message = if code == INTERNAL_ERROR_CODE {
                format!("pool rejected transaction: [{hash}]: {reason}")
            } else {
                reason.into()
            };
            let error = eyre::Report::new(Arc::new(ClientError::Call(ErrorObjectOwned::owned(
                code, message, None::<()>,
            ))));
            assert_eq!(BuilderIngressInjector::is_obsolete(&error, hash), expected, "{reason}");
        }
        assert!(!BuilderIngressInjector::is_obsolete(
            &eyre::Report::new(Arc::new(ClientError::RequestTimeout)),
            hash
        ));
    }
}
