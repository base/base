//! Chain-anchored resolution of pending nonce slots.

use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::Provider;
use base_runtime::{Runtime, RuntimeTimeout};
use futures::{StreamExt, stream};

use super::pending::{PublishedAttempt, SweepOutcome, SweepResolution, SweepTarget};
use crate::{SubmissionId, TxManagerError, TxManagerResult, TxMetrics, error::RpcErrorClassifier};

/// Maximum receipt and canonical-block queries in flight per sweep stage.
pub const MAX_CONCURRENT_SWEEP_QUERIES: usize = 8;
/// Stable missing-receipt snapshots required before declaring supersession.
pub const SUPERSESSION_OBSERVATIONS: u8 = 2;

/// Evidence accumulated before treating a consumed nonce as externally superseded.
#[derive(Debug, Clone, Copy)]
pub struct SupersessionEvidence {
    /// Attempt-history length observed by the latest sweep.
    pub attempt_count: usize,
    /// Consecutive stable sweeps with nonce consumed and no known receipt.
    pub observations: u8,
}

/// Resolves committed nonce slots against a confirmed chain snapshot.
#[derive(Debug, Clone)]
pub struct ChainSweeper<P, R> {
    /// Chain reader used for canonical observations.
    provider: P,
    /// Runtime used to bound every RPC request.
    runtime: R,
    /// Managed account whose confirmed nonce proves slot consumption.
    address: alloy_primitives::Address,
    /// Confirmation depth required before a slot can resolve.
    num_confirmations: u64,
    /// Maximum duration of each chain query.
    network_timeout: Duration,
    /// Metrics sink for RPC failures.
    metrics: Arc<dyn TxMetrics>,
    /// Stable missing-receipt evidence keyed by logical submission.
    supersession_evidence: Arc<Mutex<HashMap<SubmissionId, SupersessionEvidence>>>,
}

impl<P, R> ChainSweeper<P, R>
where
    P: Provider + Clone + Debug + Send + Sync + 'static,
    R: Runtime,
{
    /// Creates a chain sweeper.
    pub fn new(
        provider: P,
        runtime: R,
        address: alloy_primitives::Address,
        num_confirmations: u64,
        network_timeout: Duration,
        metrics: Arc<dyn TxMetrics>,
    ) -> Self {
        Self {
            provider,
            runtime,
            address,
            num_confirmations,
            network_timeout,
            metrics,
            supersession_evidence: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Reads the account nonce at the latest canonical block.
    pub async fn latest_nonce(&self) -> TxManagerResult<u64> {
        RuntimeTimeout::run(
            &self.runtime,
            self.network_timeout,
            self.provider.get_transaction_count(self.address).latest(),
        )
        .await
        .map_err(|_| self.rpc_error("latest transaction count timed out"))?
        .map_err(|error| self.classify_rpc(&error))
    }

    /// Resolves the longest confirmed prefix represented by `targets`.
    ///
    /// The account nonce is read against a canonical block hash at the required
    /// confirmation depth. A slot below that nonce is known to be consumed;
    /// receipts then determine whether one of our versions consumed it.
    pub async fn sweep(&self, targets: Vec<SweepTarget>) -> TxManagerResult<Vec<SweepResolution>> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }

        // Phase 1: anchor both confirmation height and account nonce to one
        // canonical block hash. Latest-state reads are insufficient under reorg.
        let tip = RuntimeTimeout::run(
            &self.runtime,
            self.network_timeout,
            self.provider.get_block_number(),
        )
        .await
        .map_err(|_| self.rpc_error("block number query timed out"))?
        .map_err(|error| self.classify_rpc(&error))?;
        let confirmed_height = tip.saturating_add(1).saturating_sub(self.num_confirmations);
        let block = RuntimeTimeout::run(
            &self.runtime,
            self.network_timeout,
            self.provider.get_block_by_number(BlockNumberOrTag::Number(confirmed_height)),
        )
        .await
        .map_err(|_| self.rpc_error("confirmed block query timed out"))?
        .map_err(|error| self.classify_rpc(&error))?
        .ok_or_else(|| self.rpc_error("confirmed block not found"))?;
        let confirmed_nonce = RuntimeTimeout::run(
            &self.runtime,
            self.network_timeout,
            self.provider.get_transaction_count(self.address).hash_canonical(block.header.hash),
        )
        .await
        .map_err(|_| self.rpc_error("confirmed transaction count timed out"))?
        .map_err(|error| self.classify_rpc(&error))?;

        // Phase 2: only nonces below the anchored account nonce are proven
        // consumed. Resolve them concurrently but retain target order.
        let consumed = targets.into_iter().take_while(|target| target.nonce < confirmed_nonce);
        let results =
            stream::iter(consumed.map(|target| self.resolve_target(target, confirmed_height)))
                .buffered(MAX_CONCURRENT_SWEEP_QUERIES)
                .collect::<Vec<_>>()
                .await;

        // Phase 3: preserve prefix safety. One unresolved target prevents every
        // later slot from being removed even when its RPC work completed.
        let mut resolutions = Vec::new();
        for result in results {
            let Some(resolution) = result? else {
                break;
            };
            resolutions.push(resolution);
        }
        Ok(resolutions)
    }

    /// Resolves one consumed nonce from newest known hash to oldest.
    ///
    /// Absence of a canonical receipt is not immediately treated as
    /// supersession because nonce and receipt RPC views can be temporarily
    /// inconsistent.
    pub async fn resolve_target(
        &self,
        target: SweepTarget,
        confirmed_height: u64,
    ) -> TxManagerResult<Option<SweepResolution>> {
        let receipt_queries = target
            .attempts
            .iter()
            .rev()
            .copied()
            .map(|attempt| self.canonical_receipt(attempt, confirmed_height));
        let results = stream::iter(receipt_queries)
            .buffered(MAX_CONCURRENT_SWEEP_QUERIES)
            .collect::<Vec<_>>()
            .await;
        for result in results {
            if let Some(outcome) = result? {
                self.supersession_evidence.lock().unwrap().remove(&target.submission_id);
                return Ok(Some(SweepResolution {
                    submission_id: target.submission_id,
                    attempt_count: target.attempts.len(),
                    outcome,
                }));
            }
        }

        // Require repeated observations with the same attempt history before
        // concluding that an unknown transaction consumed this nonce.
        if !self.observe_supersession(&target) {
            return Ok(None);
        }
        Ok(Some(SweepResolution {
            submission_id: target.submission_id,
            attempt_count: target.attempts.len(),
            outcome: SweepOutcome::Superseded,
        }))
    }

    /// Returns a receipt only when its block is canonical and deep enough.
    pub async fn canonical_receipt(
        &self,
        attempt: PublishedAttempt,
        confirmed_height: u64,
    ) -> TxManagerResult<Option<SweepOutcome>> {
        let receipt = RuntimeTimeout::run(
            &self.runtime,
            self.network_timeout,
            self.provider.get_transaction_receipt(attempt.hash),
        )
        .await
        .map_err(|_| self.rpc_error("transaction receipt query timed out"))?
        .map_err(|error| self.classify_rpc(&error))?;
        let Some(receipt) = receipt else {
            return Ok(None);
        };
        let (Some(block_number), Some(block_hash)) = (receipt.block_number, receipt.block_hash)
        else {
            return Ok(None);
        };
        if block_number > confirmed_height {
            return Ok(None);
        }

        // A receipt can survive briefly after its block is reorged out. Resolve
        // its block number again and compare the canonical hash explicitly.
        let canonical_block = RuntimeTimeout::run(
            &self.runtime,
            self.network_timeout,
            self.provider.get_block_by_number(BlockNumberOrTag::Number(block_number)),
        )
        .await
        .map_err(|_| self.rpc_error("receipt block query timed out"))?
        .map_err(|error| self.classify_rpc(&error))?;
        if canonical_block.as_ref().is_none_or(|block| block.header.hash != block_hash) {
            return Ok(None);
        }
        Ok(Some(SweepOutcome::Confirmed { kind: attempt.kind, receipt: Box::new(receipt) }))
    }

    /// Records stable missing-receipt evidence for conservative supersession.
    pub fn observe_supersession(&self, target: &SweepTarget) -> bool {
        let mut evidence = self.supersession_evidence.lock().unwrap();
        let observation = evidence.entry(target.submission_id).or_insert(SupersessionEvidence {
            attempt_count: target.attempts.len(),
            observations: 0,
        });
        if observation.attempt_count != target.attempts.len() {
            observation.attempt_count = target.attempts.len();
            observation.observations = 0;
        }
        observation.observations = observation.observations.saturating_add(1);
        if observation.observations < SUPERSESSION_OBSERVATIONS {
            return false;
        }
        evidence.remove(&target.submission_id);
        true
    }

    /// Classifies a transport error and records infrastructure failures.
    pub fn classify_rpc(&self, error: &alloy_transport::TransportError) -> TxManagerError {
        let classified = RpcErrorClassifier::classify_rpc_error(error);
        if classified.is_rpc_error() {
            self.metrics.record_rpc_error();
        }
        classified
    }

    /// Creates a sanitized local RPC error and records it.
    pub fn rpc_error(&self, message: &str) -> TxManagerError {
        self.metrics.record_rpc_error();
        TxManagerError::Rpc(message.to_string())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};
    use alloy_provider::RootProvider;
    use base_runtime::TokioRuntime;

    use super::*;
    use crate::NoopTxMetrics;

    fn sweeper() -> ChainSweeper<RootProvider, TokioRuntime> {
        ChainSweeper::new(
            RootProvider::new_http("http://127.0.0.1:1".parse().unwrap()),
            TokioRuntime::new(),
            Address::ZERO,
            1,
            Duration::from_secs(1),
            Arc::new(NoopTxMetrics),
        )
    }

    fn target(attempts: usize) -> SweepTarget {
        SweepTarget {
            submission_id: SubmissionId::new(1),
            nonce: 0,
            attempts: (0..attempts)
                .map(|index| PublishedAttempt {
                    version: super::super::pending::VersionId::INITIAL,
                    kind: super::super::pending::VersionKind::Original,
                    hash: B256::with_last_byte(index as u8),
                })
                .collect(),
        }
    }

    #[test]
    fn supersession_requires_repeated_stable_observations() {
        let sweeper = sweeper();
        assert!(!sweeper.observe_supersession(&target(1)));
        assert!(sweeper.observe_supersession(&target(1)));
    }

    #[test]
    fn a_new_attempt_resets_supersession_evidence() {
        let sweeper = sweeper();
        assert!(!sweeper.observe_supersession(&target(1)));
        assert!(!sweeper.observe_supersession(&target(2)));
        assert!(sweeper.observe_supersession(&target(2)));
    }
}
