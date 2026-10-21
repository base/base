use std::{collections::HashSet, ops::RangeInclusive, time::Duration};

use alloy_network::TxSigner;
use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Filter;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use base_common_consensus::Predeploys;
use base_withdrawer::{
    Network, ProofSystem, ResolvedProofSystem, Secret, SignerSource, SubmitError, Withdrawal,
    WithdrawalPortal, WithdrawalRpc, WithdrawalSender, WithdrawalSigner, WithdrawalStatus,
    Withdrawer, WithdrawerConfig, WithdrawerError,
};
use eyre::{WrapErr, ensure};
use tokio::time::{Instant, timeout_at};
use tracing::{error, info, warn};

use crate::{ConfirmArgs, CrossDomainMessenger, FeeDisburser, StandardBridge};

/// Inclusive L2 log-query ranges.
#[derive(Debug)]
pub struct ConfirmWindow;

impl ConfirmWindow {
    /// Rounds the duration up to blocks, clamps at genesis, and splits into inclusive chunks.
    pub fn ranges(
        current: u64,
        lookback: Duration,
        block_time: Duration,
        chunk: u64,
    ) -> eyre::Result<Vec<RangeInclusive<u64>>> {
        ensure!(!lookback.is_zero(), "lookback must be positive");
        ensure!(!block_time.is_zero(), "L2 block time must be positive");
        let blocks =
            u64::try_from(lookback.as_nanos().div_ceil(block_time.as_nanos())).unwrap_or(u64::MAX);
        eyre::ensure!(chunk > 0, "number of blocks per query must be positive");
        let mut start = current.saturating_sub(blocks);
        let mut ranges = Vec::new();
        loop {
            let end = start.saturating_add(chunk - 1).min(current);
            ranges.push(start..=end);
            if end == current {
                return Ok(ranges);
            }
            start = end + 1;
        }
    }
}

/// L1 submission identity and the manager, absent for dry runs.
#[derive(Debug)]
pub struct ConfirmSender<'a> {
    /// Address used for proof selection and gas estimation.
    pub address: Address,
    /// Shared manager for serialized L1 sends.
    pub manager: Option<&'a WithdrawalSender>,
}

/// Readiness timing from the L1 head used by the withdrawal status check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmTiming {
    /// Earliest known ready timestamp; game resolution can delay it further.
    pub ready_at: u64,
    /// L1 head timestamp used to decide readiness.
    pub l1_timestamp: u64,
    /// Game resolution is required before its finality delay can start.
    pub waiting_for_game: bool,
}

impl ConfirmTiming {
    /// Logs the known delay without another RPC request.
    pub fn log(&self, hash: B256) {
        info!(withdrawal_hash = %hash, ready_at = self.ready_at, l1_timestamp = self.l1_timestamp,
            seconds_until_earliest_ready = self.ready_at.saturating_sub(self.l1_timestamp),
            waiting_for_game = self.waiting_for_game,
            "withdrawal pending; game resolution may delay readiness further");
    }
}

/// Result of processing one fee withdrawal.
#[derive(Debug)]
pub enum ConfirmOutcome {
    /// Finalization submitted (`None` for dry runs, which prepare without broadcasting).
    Finalized(Option<B256>),
    /// Proof submitted (`None` for dry runs); pending until a later run.
    Proven(Option<B256>),
    /// Already finalized at the initial check or after a submission error.
    AlreadyFinalized,
    /// Waiting for maturity or game finality; absent timing means a new proof is needed.
    Pending {
        /// Readiness from the status check, absent when the proof was invalidated.
        timing: Option<ConfirmTiming>,
    },
    /// Processing or the per-withdrawal deadline failed, retaining the error chain.
    Failed(eyre::Report),
}

impl ConfirmOutcome {
    /// Whether the withdrawal needs a later run.
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending { .. } | Self::Proven(_))
    }
}

/// Processing results, separate from failures setting up or scanning the job.
#[derive(Debug, Default)]
pub struct ConfirmReport {
    /// Disbursal transaction hashes whose receipts could not yield fee withdrawals.
    pub receipt_failures: Vec<(B256, eyre::Report)>,
    /// Withdrawal hash, originating disbursal transaction hash, and processing outcome.
    pub withdrawals: Vec<(B256, B256, ConfirmOutcome)>,
}

impl ConfirmReport {
    /// Fails the process if processing failed, preserving a source and typed withdrawer errors.
    pub fn into_result(self) -> eyre::Result<()> {
        let errors = self
            .receipt_failures
            .into_iter()
            .map(|(_, error)| error)
            .chain(self.withdrawals.into_iter().filter_map(|(_, _, outcome)| match outcome {
                ConfirmOutcome::Failed(error) => Some(error),
                _ => None,
            }))
            .collect::<Vec<_>>();
        let count = errors.len();
        if let Some(error) =
            errors.into_iter().max_by_key(|error| error.downcast_ref::<WithdrawerError>().is_some())
        {
            return Err(error)
                .wrap_err(format!("{count} withdrawals or disbursal receipts failed"));
        }
        Ok(())
    }
}

/// Confirms recent fee disbursals using one serialized L1 signer.
#[derive(Debug)]
pub struct ConfirmJob {
    /// Command configuration.
    pub args: ConfirmArgs,
}

impl ConfirmJob {
    /// Validates the command and constructs a custom fault-proof withdrawer configuration.
    pub fn config(&self) -> eyre::Result<WithdrawerConfig> {
        ensure!(
            [self.args.l1_rpc_url.scheme(), self.args.l2_rpc_url.scheme()]
                .into_iter()
                .all(|scheme| matches!(scheme, "http" | "https")),
            "RPC URLs must use HTTP or HTTPS"
        );
        ensure!(
            [self.args.l1_optimism_portal_address, self.args.l2_fee_disburser_address]
                .into_iter()
                .all(|address| address != Address::ZERO),
            "contract addresses must not be zero"
        );
        ensure!(!self.args.lookback.is_zero(), "lookback must be positive");
        ensure!(!self.args.l2_block_time.is_zero(), "L2 block time must be positive");
        ensure!(self.args.num_blocks_per_query > 0, "number of blocks per query must be positive");
        ensure!(!self.args.withdrawal_timeout.is_zero(), "withdrawal timeout must be positive");
        let signer: PrivateKeySigner = self.args.job_runner_key.0.parse().map_err(|_| {
            eyre::eyre!("JOB_RUNNER_KEY must be a valid 32-byte secp256k1 key in hex")
        })?;
        let config = WithdrawerConfig {
            l1_rpc_url: self.args.l1_rpc_url.clone(),
            network: Network {
                l2_rpc_url: self.args.l2_rpc_url.clone(),
                portal: self.args.l1_optimism_portal_address,
                proofs: ProofSystem::FaultProofs { expected: None },
                l1_chain_id: None,
            },
            withdrawal: B256::ZERO,
            signer: SignerSource::PrivateKey(Secret(signer.to_bytes())),
            gas_limit: None,
            tx_manager: self.args.tx_manager.clone().try_into()?,
            dry_run: self.args.dry_run,
            timeout: self.args.withdrawal_timeout,
            max_retries: 3,
            retry_delay: Duration::from_secs(2),
        };
        config.submission_request_timeout().map_err(|_| {
            eyre::eyre!(
                "FEE_JOBS_TX_MANAGER_NETWORK_TIMEOUT / --tx-manager.network-timeout must leave time for HTTP attempts after the fixed 2 s retry delay"
            )
        })?;
        Ok(config)
    }

    /// Queries disbursal transactions in block/log order, deduplicating repeated hashes.
    pub async fn disbursals(&self, rpc: &WithdrawalRpc) -> eyre::Result<Vec<B256>> {
        let current = rpc.l2.get_block_number().await.map_err(WithdrawerError::from)?;
        let ranges = ConfirmWindow::ranges(
            current,
            self.args.lookback,
            self.args.l2_block_time,
            self.args.num_blocks_per_query,
        )?;
        let mut transactions = Vec::new();
        let mut seen = HashSet::new();
        for range in ranges {
            info!(start = %range.start(), end = %range.end(), "querying fee disbursals");
            let filter = Filter::new()
                .address(self.args.l2_fee_disburser_address)
                .event_signature(FeeDisburser::FeesDisbursed::SIGNATURE_HASH)
                .from_block(*range.start())
                .to_block(*range.end());
            let mut logs = rpc.l2.get_logs(&filter).await.map_err(WithdrawerError::from)?;
            logs.sort_by_key(|log| (log.block_number, log.log_index));
            for log in logs {
                let hash = log
                    .transaction_hash
                    .ok_or_else(|| eyre::eyre!("fee disbursal log has no transaction hash"))?;
                if seen.insert(hash) {
                    transactions.push(hash);
                }
            }
        }
        info!(count = transactions.len(), ?transactions, "found fee disbursal transactions");
        Ok(transactions)
    }

    /// Extracts only the `FeeDisburser`'s authenticated ETH bridge withdrawals from a receipt.
    pub async fn withdrawals(
        &self,
        rpc: &WithdrawalRpc,
        transaction: B256,
    ) -> eyre::Result<Vec<Withdrawal>> {
        let receipt = rpc
            .l2
            .get_transaction_receipt(transaction)
            .await
            .map_err(WithdrawerError::from)?
            .ok_or(WithdrawerError::MissingReceipt)?;
        let withdrawals = Withdrawal::from_receipt(&receipt)
            .map_err(WithdrawerError::from)?
            .into_iter()
            .filter(|withdrawal| {
                let matches = self.is_fee_withdrawal(withdrawal);
                if !matches {
                    warn!(withdrawal_hash = %withdrawal.hash(), "skipping withdrawal not sent by the FeeDisburser");
                }
                matches
            })
            .collect::<Vec<_>>();
        ensure!(!withdrawals.is_empty(), "receipt has no FeeDisburser bridge withdrawals");
        Ok(withdrawals)
    }

    /// Authenticates the bridge sender and original caller inside the messenger envelope.
    pub fn is_fee_withdrawal(&self, withdrawal: &Withdrawal) -> bool {
        if withdrawal.transaction.sender != Predeploys::L2_CROSS_DOMAIN_MESSENGER {
            return false;
        }
        let Ok(relay) = CrossDomainMessenger::relayMessageCall::abi_decode_validate(
            &withdrawal.transaction.data,
        ) else {
            return false;
        };
        relay._sender == Predeploys::L2_STANDARD_BRIDGE
            && StandardBridge::finalizeBridgeETHCall::abi_decode_validate(&relay._message)
                .is_ok_and(|call| call._from == self.args.l2_fee_disburser_address)
    }

    /// Processes one withdrawal within its deadline, rechecking only unpublished finalizations.
    pub async fn confirm_withdrawal(
        &self,
        withdrawer: &Withdrawer,
        rpc: &WithdrawalRpc,
        proofs: ResolvedProofSystem,
        sender: &ConfirmSender<'_>,
        withdrawal: &Withdrawal,
    ) -> ConfirmOutcome {
        let deadline = Instant::now() + self.args.withdrawal_timeout;
        let result: eyre::Result<ConfirmOutcome> = async {
            let hash = withdrawal.hash();
            let submitter = sender.address;
            let status =
                timeout_at(deadline, withdrawer.status(rpc, proofs, withdrawal, submitter))
                    .await
                    .map_err(WithdrawerError::Timeout)??;
            info!(withdrawal_hash = %hash, ?status, "withdrawal decision");
            match status {
                WithdrawalStatus::Finalized => {
                    info!(withdrawal_hash = %hash, "withdrawal already finalized");
                    Ok(ConfirmOutcome::AlreadyFinalized)
                }
                WithdrawalStatus::Paused => {
                    Err(eyre::eyre!("withdrawal portal or anchor state registry is paused"))
                }
                WithdrawalStatus::NotReady { ready_at, l1_timestamp, waiting_for_game } => {
                    let timing = ConfirmTiming { ready_at, l1_timestamp, waiting_for_game };
                    timing.log(hash);
                    Ok(ConfirmOutcome::Pending { timing: Some(timing) })
                }
                WithdrawalStatus::Prove => {
                    let calldata =
                        timeout_at(deadline, withdrawer.prove_calldata(rpc, proofs, withdrawal))
                            .await
                            .map_err(WithdrawerError::Timeout)??;
                    let transaction_hash = withdrawer
                        .submit(rpc, submitter, sender.manager, calldata, None, deadline)
                        .await
                        .map_err(WithdrawerError::from)?;
                    if self.args.dry_run {
                        info!(withdrawal_hash = %hash, "dry run proof prepared");
                    } else {
                        info!(withdrawal_hash = %hash, "withdrawal pending, just proven");
                    }
                    Ok(ConfirmOutcome::Proven(transaction_hash))
                }
                WithdrawalStatus::Finalize => {
                    let calldata = WithdrawalPortal::finalizeWithdrawalTransactionCall {
                        withdrawal: withdrawal.transaction.clone(),
                    }
                    .abi_encode()
                    .into();
                    match withdrawer
                        .submit(rpc, submitter, sender.manager, calldata, Some(hash), deadline)
                        .await
                    {
                        Ok(transaction_hash) => {
                            if self.args.dry_run {
                                info!(withdrawal_hash = %hash, "dry run finalization prepared");
                            } else {
                                info!(
                                    withdrawal_hash = %hash,
                                    ?transaction_hash,
                                    "withdrawal finalized"
                                );
                            }
                            Ok(ConfirmOutcome::Finalized(transaction_hash))
                        }
                        Err(error @ SubmitError::MaybeIncluded { .. }) => {
                            Err(WithdrawerError::from(error).into())
                        }
                        Err(SubmitError::NotSent(error)) => {
                            if Instant::now() >= deadline {
                                return Err(error.into());
                            }
                            let recheck = timeout_at(
                                deadline,
                                withdrawer.status(rpc, proofs, withdrawal, submitter),
                            )
                            .await
                            .map_err(WithdrawerError::Timeout)
                            .wrap_err_with(|| format!("{error:#}"))?;
                            match recheck {
                                Ok(WithdrawalStatus::Finalized) => {
                                    info!(withdrawal_hash = %hash, "withdrawal already finalized");
                                    Ok(ConfirmOutcome::AlreadyFinalized)
                                }
                                Ok(WithdrawalStatus::NotReady {
                                    ready_at,
                                    l1_timestamp,
                                    waiting_for_game,
                                }) => {
                                    let timing =
                                        ConfirmTiming { ready_at, l1_timestamp, waiting_for_game };
                                    info!(
                                        withdrawal_hash = %hash,
                                        error = %format_args!("{error:#}"),
                                        "withdrawal not ready"
                                    );
                                    timing.log(hash);
                                    Ok(ConfirmOutcome::Pending { timing: Some(timing) })
                                }
                                Ok(WithdrawalStatus::Prove) => {
                                    info!(
                                        withdrawal_hash = %hash,
                                        error = %format_args!("{error:#}"),
                                        "withdrawal pending; a new proof is needed"
                                    );
                                    Ok(ConfirmOutcome::Pending { timing: None })
                                }
                                Ok(_) => Err(error.into()),
                                Err(recheck_error) => Err(error).wrap_err(format!(
                                    "could not recheck withdrawal status: {recheck_error:#}"
                                )),
                            }
                        }
                    }
                }
            }
        }
        .await;
        result.unwrap_or_else(ConfirmOutcome::Failed)
    }

    /// Collects every processing outcome; only setup or disbursal scan failures return `Err`.
    pub async fn run(self) -> eyre::Result<ConfirmReport> {
        let config = self.config()?;
        let rpc = WithdrawalRpc::new(&config).map_err(WithdrawerError::from)?;
        let withdrawer = Withdrawer::new(config);
        let proofs = withdrawer.resolve_proof_system(&rpc).await?;
        let signer = WithdrawalSigner::new(&withdrawer.config.signer)
            .await
            .map_err(WithdrawerError::from)?;
        let submitter = signer.address();
        let transactions = self.disbursals(&rpc).await?;
        let manager =
            if self.args.dry_run { None } else { Some(withdrawer.manager(signer).await?) };
        let sender = ConfirmSender { address: submitter, manager: manager.as_ref() };
        let mut report = ConfirmReport::default();
        for transaction in transactions {
            let withdrawals = match self.withdrawals(&rpc, transaction).await {
                Ok(withdrawals) => withdrawals,
                Err(error) => {
                    error!(transaction_hash = %transaction, error = %format_args!("{error:#}"), "could not extract disbursal withdrawals");
                    report.receipt_failures.push((transaction, error));
                    continue;
                }
            };
            for withdrawal in withdrawals {
                let hash = withdrawal.hash();
                let result =
                    self.confirm_withdrawal(&withdrawer, &rpc, proofs, &sender, &withdrawal).await;
                if let ConfirmOutcome::Failed(error) = &result {
                    error!(withdrawal_hash = %hash, transaction_hash = %transaction, error = %format_args!("{error:#}"), "withdrawal failed");
                }
                report.withdrawals.push((hash, transaction, result));
            }
        }
        if let Some(manager) = manager {
            manager.manager.close();
        }
        let pending: Vec<_> = report
            .withdrawals
            .iter()
            .filter(|(_, _, outcome)| outcome.is_pending())
            .map(|(hash, _, _)| *hash)
            .collect();
        let failed: Vec<_> = report
            .withdrawals
            .iter()
            .filter(|(_, _, outcome)| matches!(outcome, ConfirmOutcome::Failed(_)))
            .map(|(hash, _, _)| *hash)
            .collect();
        let receipt_failures: Vec<_> =
            report.receipt_failures.iter().map(|(hash, _)| *hash).collect();
        info!(count = pending.len(), hashes = ?pending, "pending withdrawals");
        info!(count = failed.len(), hashes = ?failed, "failed withdrawals");
        info!(count = receipt_failures.len(), hashes = ?receipt_failures, "failed disbursal receipts");
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Future, pending, ready},
        io::{self, Write},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use alloy_primitives::{Bytes, U256};
    use alloy_rpc_types_eth::{Block, Transaction};
    use axum::{Json, Router, routing::post};
    use base_proof_contracts::IAnchorStateRegistry;
    use base_withdrawer::{
        FaultProofContracts, FaultProofPortal, MessagePassed, WithdrawalDisputeGame,
        WithdrawalTransaction,
    };
    use rstest::rstest;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, sync::Notify, task::JoinHandle};
    use tracing::instrument::WithSubscriber;
    use url::Url;

    use super::*;

    #[derive(Clone)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Server {
        url: Url,
        task: JoinHandle<()>,
    }

    impl Server {
        async fn new(handler: impl Fn(Value) -> Value + Send + Sync + 'static) -> Self {
            Self::new_async(move |request| ready(handler(request))).await
        }

        async fn new_async<F: Future<Output = Value> + Send + 'static>(
            handler: impl Fn(Value) -> F + Send + Sync + 'static,
        ) -> Self {
            let handler = Arc::new(handler);
            let router = Router::new().fallback(post(move |Json(request): Json<Value>| {
                let response = handler(request.clone());
                async move {
                    let mut response = response.await;
                    response["jsonrpc"] = json!("2.0");
                    response["id"] = request["id"].clone();
                    Json(response)
                }
            }));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            Self { url, task }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn job(url: &Url) -> ConfirmJob {
        ConfirmJob {
            args: ConfirmArgs {
                tx_manager: crate::TxManagerCli { num_confirmations: 1, ..Default::default() },
                l1_rpc_url: url.clone(),
                l2_rpc_url: url.clone(),
                l1_optimism_portal_address: Address::repeat_byte(1),
                l2_fee_disburser_address: Address::repeat_byte(4),
                job_runner_key: Secret("01".repeat(32)),
                lookback: Duration::from_secs(18),
                l2_block_time: Duration::from_secs(2),
                num_blocks_per_query: 4,
                dry_run: true,
                withdrawal_timeout: Duration::from_secs(10),
            },
        }
    }

    #[tokio::test]
    async fn initial_status_read_respects_withdrawal_deadline() {
        let started = Arc::new(Notify::new());
        let captured = Arc::clone(&started);
        let server = Server::new_async(move |request| {
            let started = Arc::clone(&captured);
            async move {
                assert_eq!(request["method"], "eth_call");
                let data: Bytes =
                    serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
                assert_eq!(data[..4], WithdrawalPortal::finalizedWithdrawalsCall::SELECTOR);
                started.notify_one();
                pending().await
            }
        })
        .await;
        let job = job(&server.url);
        let mut config = job.config().unwrap();
        // The HTTP attempt must outlive the withdrawal deadline, without relying on retries.
        config.timeout = Duration::from_secs(60);
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let withdrawer = Withdrawer::new(config);
        let receipt = serde_json::from_value(disbursal_receipt(
            Predeploys::L2_CROSS_DOMAIN_MESSENGER,
            Predeploys::L2_STANDARD_BRIDGE,
            job.args.l2_fee_disburser_address,
        ))
        .unwrap();
        let withdrawal = Withdrawal::from_receipt(&receipt).unwrap().remove(0);
        let proofs = ResolvedProofSystem::FaultProofs(FaultProofContracts {
            dispute_game_factory: Address::repeat_byte(2),
            anchor_state_registry: Address::repeat_byte(3),
        });
        let sender = ConfirmSender { address: Address::repeat_byte(9), manager: None };
        let confirmation = job.confirm_withdrawal(&withdrawer, &rpc, proofs, &sender, &withdrawal);
        tokio::pin!(confirmation);
        tokio::select! {
            () = started.notified() => {},
            outcome = &mut confirmation => panic!("status read did not hang: {outcome:?}"),
        }
        tokio::time::pause();
        tokio::time::advance(job.args.withdrawal_timeout).await;
        let outcome = tokio::time::timeout(Duration::from_secs(1), confirmation)
            .await
            .expect("confirm must return at the withdrawal deadline, before the HTTP timeout");
        let ConfirmOutcome::Failed(error) = outcome else { panic!("{outcome:?}") };
        assert!(
            matches!(error.downcast_ref::<WithdrawerError>(), Some(WithdrawerError::Timeout(_))),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn recheck_deadline_preserves_typed_timeout() {
        let started = Arc::new(Notify::new());
        let captured_started = Arc::clone(&started);
        let status_reads = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&status_reads);
        let server = Server::new_async(move |request| {
            let status_reads = Arc::clone(&captured);
            let started = Arc::clone(&captured_started);
            async move {
                match request["method"].as_str().unwrap() {
                    "eth_getBlockByNumber" => {
                        let mut block = Block::<Transaction>::default();
                        block.header.inner.timestamp = 1000;
                        json!({"result": block})
                    }
                    "eth_call" => {
                        let data: Bytes =
                            serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
                        if data[..4] == WithdrawalPortal::finalizedWithdrawalsCall::SELECTOR
                            && status_reads.fetch_add(1, Ordering::Relaxed) > 0
                        {
                            started.notify_one();
                            return pending().await;
                        }
                        macro_rules! reply {
                            ($call:ty, $($value:expr),* $(,)?) => {
                                if data[..4] == <$call>::SELECTOR {
                                    return json!({"result": Bytes::from(
                                        <$call>::abi_encode_returns_tuple(&($($value,)*))
                                    )});
                                }
                            };
                        }
                        reply!(WithdrawalPortal::finalizedWithdrawalsCall, false);
                        reply!(IAnchorStateRegistry::pausedCall, false);
                        reply!(
                            FaultProofPortal::provenWithdrawalsCall,
                            Address::repeat_byte(8),
                            800u64
                        );
                        reply!(IAnchorStateRegistry::isGameBlacklistedCall, false);
                        reply!(IAnchorStateRegistry::isGameRetiredCall, false);
                        reply!(IAnchorStateRegistry::isGameProperCall, true);
                        reply!(IAnchorStateRegistry::isGameRespectedCall, true);
                        reply!(WithdrawalDisputeGame::statusCall, 2u8);
                        reply!(FaultProofPortal::proofMaturityDelaySecondsCall, U256::from(100));
                        reply!(WithdrawalDisputeGame::resolvedAtCall, 700u64);
                        reply!(
                            IAnchorStateRegistry::disputeGameFinalityDelaySecondsCall,
                            U256::from(200)
                        );
                        reply!(IAnchorStateRegistry::isGameFinalizedCall, true);
                        reply!(FaultProofPortal::checkWithdrawalCall,);
                        panic!("unexpected contract selector {data}");
                    }
                    method => panic!("unexpected RPC method {method}"),
                }
            }
        })
        .await;
        let mut job = job(&server.url);
        job.args.dry_run = false;
        let mut config = job.config().unwrap();
        // Keep the HTTP attempt alive beyond the job deadline without relying on retries.
        config.timeout = Duration::from_secs(60);
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let withdrawer = Withdrawer::new(config);
        let receipt = serde_json::from_value(disbursal_receipt(
            Predeploys::L2_CROSS_DOMAIN_MESSENGER,
            Predeploys::L2_STANDARD_BRIDGE,
            job.args.l2_fee_disburser_address,
        ))
        .unwrap();
        let withdrawal = Withdrawal::from_receipt(&receipt).unwrap().remove(0);
        let proofs = ResolvedProofSystem::FaultProofs(FaultProofContracts {
            dispute_game_factory: Address::repeat_byte(2),
            anchor_state_registry: Address::repeat_byte(3),
        });
        // A missing manager fails before broadcast, leaving time for the status recheck.
        let sender = ConfirmSender { address: Address::repeat_byte(9), manager: None };
        let confirmation = job.confirm_withdrawal(&withdrawer, &rpc, proofs, &sender, &withdrawal);
        tokio::pin!(confirmation);
        tokio::select! {
            () = started.notified() => {},
            outcome = &mut confirmation => panic!("recheck did not hang: {outcome:?}"),
        }
        tokio::time::pause();
        tokio::time::advance(job.args.withdrawal_timeout).await;
        let outcome = tokio::time::timeout(Duration::from_secs(1), confirmation)
            .await
            .expect("recheck must return at the withdrawal deadline, before the HTTP timeout");
        let ConfirmOutcome::Failed(error) = outcome else { panic!("{outcome:?}") };
        assert_eq!(status_reads.load(Ordering::Relaxed), 2);
        assert!(
            matches!(error.downcast_ref::<WithdrawerError>(), Some(WithdrawerError::Timeout(_))),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("missing withdrawal transaction manager"));
    }

    #[test]
    fn rejects_submission_deadline_at_or_below_retry_delay() {
        let mut job = job(&"http://localhost:8545".parse().unwrap());
        for timeout in [Duration::from_secs(1), Duration::from_secs(2)] {
            job.args.tx_manager.network_timeout = timeout;
            let error = job.config().unwrap_err().to_string();
            assert!(error.contains("FEE_JOBS_TX_MANAGER_NETWORK_TIMEOUT"));
            assert!(error.contains("--tx-manager.network-timeout"));
            assert!(error.contains("fixed 2 s retry delay"));
            assert!(!error.contains("--retry-delay"));
        }
    }

    fn disbursal_receipt(sender: Address, relay_sender: Address, from: Address) -> Value {
        let mut receipt: Value = serde_json::from_str(include_str!(
            "../../withdrawer/fixtures/synthetic-deposit-withdrawal-receipt.json"
        ))
        .unwrap();
        let log: alloy_rpc_types_eth::Log =
            serde_json::from_value(receipt["logs"][0].clone()).unwrap();
        let mut event = MessagePassed::decode_log(&log.inner).unwrap().data;
        event.sender = sender;
        event.data = CrossDomainMessenger::relayMessageCall {
            _nonce: event.nonce,
            _sender: relay_sender,
            _target: Address::repeat_byte(5),
            _value: event.value,
            _minGasLimit: U256::ZERO,
            _message: StandardBridge::finalizeBridgeETHCall {
                _from: from,
                _to: Address::repeat_byte(6),
                _amount: event.value,
                _extraData: Bytes::new(),
            }
            .abi_encode()
            .into(),
        }
        .abi_encode()
        .into();
        event.withdrawalHash = Withdrawal {
            transaction: WithdrawalTransaction {
                nonce: event.nonce,
                sender: event.sender,
                target: event.target,
                value: event.value,
                gasLimit: event.gasLimit,
                data: event.data.clone(),
            },
            block_number: 1,
        }
        .hash();
        let encoded = event.encode_log_data();
        receipt["blockHash"] = json!(B256::repeat_byte(9));
        receipt["logs"][0]["topics"] = json!(encoded.topics());
        receipt["logs"][0]["data"] = json!(encoded.data);
        receipt
    }

    fn fee_log(block: u64, index: u64, transaction: B256) -> Value {
        json!({"address":Address::repeat_byte(4), "topics":[FeeDisburser::FeesDisbursed::SIGNATURE_HASH],
            "data":format!("0x{}", "00".repeat(96)), "blockNumber":format!("0x{block:x}"),
            "logIndex":format!("0x{index:x}"), "transactionIndex":"0x0", "transactionHash":transaction})
    }

    #[tokio::test]
    async fn scans_inclusive_chunks_and_deduplicates_in_block_log_order() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&queries);
        let a = B256::repeat_byte(1);
        let b = B256::repeat_byte(2);
        let c = B256::repeat_byte(3);
        let server = Server::new(move |request| match request["method"].as_str().unwrap() {
            "eth_blockNumber" => json!({"result":"0x64"}),
            "eth_getLogs" => {
                let filter = request["params"][0].clone();
                let response = match filter["fromBlock"].as_str().unwrap() {
                    "0x5b" => vec![fee_log(94, 1, c), fee_log(91, 2, a), fee_log(91, 1, a)],
                    "0x5f" => vec![],
                    "0x63" => vec![fee_log(100, 0, b), fee_log(99, 0, a)],
                    _ => unreachable!(),
                };
                captured.lock().unwrap().push(filter);
                json!({"result":response})
            }
            _ => unreachable!(),
        })
        .await;
        let job = job(&server.url);
        let rpc = WithdrawalRpc::new(&job.config().unwrap()).unwrap();
        assert_eq!(job.disbursals(&rpc).await.unwrap(), vec![a, c, b]);
        let queries = queries.lock().unwrap();
        let ranges: Vec<_> =
            queries.iter().map(|q| (q["fromBlock"].clone(), q["toBlock"].clone())).collect();
        assert_eq!(
            ranges,
            vec![
                (json!("0x5b"), json!("0x5e")),
                (json!("0x5f"), json!("0x62")),
                (json!("0x63"), json!("0x64"))
            ]
        );
        assert_eq!(queries[0]["address"], json!(job.args.l2_fee_disburser_address));
        assert_eq!(queries[0]["topics"], json!([FeeDisburser::FeesDisbursed::SIGNATURE_HASH]));
    }

    #[tokio::test]
    async fn continues_after_failed_receipts_and_withdrawals_and_returns_failure() {
        let mut receipt = disbursal_receipt(
            Predeploys::L2_CROSS_DOMAIN_MESSENGER,
            Predeploys::L2_STANDARD_BRIDGE,
            Address::repeat_byte(4),
        );
        let log: alloy_rpc_types_eth::Log =
            serde_json::from_value(receipt["logs"][0].clone()).unwrap();
        let event = MessagePassed::decode_log(&log.inner).unwrap().data;
        let second = Withdrawal {
            transaction: WithdrawalTransaction {
                nonce: event.nonce + U256::from(1),
                sender: event.sender,
                target: event.target,
                value: event.value,
                gasLimit: event.gasLimit,
                data: event.data.clone(),
            },
            block_number: 1,
        };
        let second_event = MessagePassed {
            nonce: second.transaction.nonce,
            withdrawalHash: second.hash(),
            ..event.clone()
        }
        .encode_log_data();
        let mut second_log = receipt["logs"][0].clone();
        second_log["topics"] = json!(second_event.topics());
        second_log["data"] = json!(second_event.data);
        receipt["logs"].as_array_mut().unwrap().push(second_log);
        let unrelated: Value = serde_json::from_str(include_str!(
            "../../withdrawer/fixtures/synthetic-deposit-withdrawal-receipt.json"
        ))
        .unwrap();
        let visited = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&visited);
        let server = Server::new(move |request| match request["method"].as_str().unwrap() {
            "eth_blockNumber" => json!({"result":"0x64"}),
            "eth_getLogs" => json!({"result":[fee_log(100, 0, B256::repeat_byte(1)), fee_log(100, 1, B256::repeat_byte(2)), fee_log(100, 2, B256::repeat_byte(3))]}),
            "eth_getTransactionReceipt" if request["params"][0] == json!(B256::repeat_byte(1)) => json!({"result":null}),
            "eth_getTransactionReceipt" if request["params"][0] == json!(B256::repeat_byte(3)) => json!({"result":unrelated}),
            "eth_getTransactionReceipt" => json!({"result":receipt}),
            "eth_call" => {
                let data: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
                if data.as_ref() == FaultProofPortal::disputeGameFactoryCall::SELECTOR {
                    return json!({"result":Bytes::from(FaultProofPortal::disputeGameFactoryCall::abi_encode_returns(&Address::repeat_byte(2)))});
                }
                if data.as_ref() == FaultProofPortal::anchorStateRegistryCall::SELECTOR {
                    return json!({"result":Bytes::from(FaultProofPortal::anchorStateRegistryCall::abi_encode_returns(&Address::repeat_byte(3)))});
                }
                let call = WithdrawalPortal::finalizedWithdrawalsCall::abi_decode(&data).unwrap();
                captured.lock().unwrap().push(call.withdrawalHash);
                if call.withdrawalHash == event.withdrawalHash {
                    json!({"error":{"code":3,"message":"execution reverted: https://user:password@secret.endpoint"}})
                } else {
                    json!({"result":Bytes::from(WithdrawalPortal::finalizedWithdrawalsCall::abi_encode_returns(&true))})
                }
            }
            _ => unreachable!(),
        }).await;
        let mut job = job(&server.url);
        job.args.lookback = Duration::from_nanos(1);
        let report = job.run().await.unwrap();
        assert_eq!(
            report.receipt_failures.iter().map(|(hash, _)| *hash).collect::<Vec<_>>(),
            vec![B256::repeat_byte(1), B256::repeat_byte(3)]
        );
        assert!(matches!(
            report.receipt_failures[0].1.downcast_ref::<WithdrawerError>(),
            Some(WithdrawerError::MissingReceipt)
        ));
        assert_eq!(report.withdrawals.len(), 2);
        assert_eq!(report.withdrawals[0].0, event.withdrawalHash);
        assert_eq!(report.withdrawals[0].1, B256::repeat_byte(2));
        let ConfirmOutcome::Failed(error) = &report.withdrawals[0].2 else { panic!("{report:?}") };
        assert!(matches!(
            error.downcast_ref::<WithdrawerError>(),
            Some(WithdrawerError::Request(_))
        ));
        assert_eq!(report.withdrawals[1].0, second.hash());
        assert!(matches!(report.withdrawals[1].2, ConfirmOutcome::AlreadyFinalized));
        assert!(!format!("{report:?}").contains("secret.endpoint"));
        let error = report.into_result().unwrap_err();
        assert!(error.downcast_ref::<WithdrawerError>().is_some());
        assert!(!format!("{error:#} {error:?}").contains("secret.endpoint"));
        assert_eq!(*visited.lock().unwrap(), vec![event.withdrawalHash, second.hash()]);
    }

    #[rstest]
    #[case::pending(899, false, Some(WithdrawalStatus::Finalize), Some(1), None)]
    #[case::paused(1000, true, Some(WithdrawalStatus::Finalize), None, None)]
    #[case::finalization_prepared(1000, false, Some(WithdrawalStatus::Finalize), Some(0), None)]
    #[case::finalization_submitted(
        1000,
        false,
        Some(WithdrawalStatus::Finalize),
        Some(0),
        Some("success")
    )]
    #[case::already_finalized_during_estimation(
        1000,
        false,
        Some(WithdrawalStatus::Finalized),
        Some(0),
        None
    )]
    #[case::not_ready_during_estimation(1000, false, Some(WithdrawalStatus::NotReady { ready_at: 901, l1_timestamp: 1000, waiting_for_game: false }), Some(1), None)]
    #[case::proof_invalidated_during_estimation(
        1000,
        false,
        Some(WithdrawalStatus::Prove),
        Some(1),
        None
    )]
    #[case::still_ready_during_estimation(
        1000,
        false,
        Some(WithdrawalStatus::Finalize),
        None,
        None
    )]
    #[case::paused_during_estimation(1000, false, Some(WithdrawalStatus::Paused), None, None)]
    #[case::recheck_read_fails(1000, false, None, None, None)]
    #[case::target_call_failed(
        1000,
        false,
        Some(WithdrawalStatus::Finalized),
        None,
        Some("target call failed")
    )]
    #[case::missing_event(
        1000,
        false,
        Some(WithdrawalStatus::Finalized),
        None,
        Some("no portal finalization event")
    )]
    #[case::deadline_before_broadcast(
        1000,
        false,
        Some(WithdrawalStatus::Finalized),
        None,
        Some("deadline before broadcast")
    )]
    #[case::unconfirmed(1000, false, Some(WithdrawalStatus::Finalized), None, Some("was signed"))]
    #[tokio::test]
    async fn run_handles_pending_paused_races_and_unrelated_withdrawals(
        #[case] now: u64,
        #[case] paused: bool,
        #[case] recheck: Option<WithdrawalStatus>,
        #[case] pending: Option<usize>,
        #[case] finalization_error: Option<&'static str>,
        #[values(
            None,
            Some((Address::repeat_byte(9), Predeploys::L2_STANDARD_BRIDGE, Address::repeat_byte(4))),
            Some((Predeploys::L2_CROSS_DOMAIN_MESSENGER, Address::repeat_byte(9), Address::repeat_byte(4))),
            Some((Predeploys::L2_CROSS_DOMAIN_MESSENGER, Predeploys::L2_STANDARD_BRIDGE, Address::repeat_byte(9))),
        )]
        unrelated_senders: Option<(Address, Address, Address)>,
    ) {
        let mut receipt = disbursal_receipt(
            Predeploys::L2_CROSS_DOMAIN_MESSENGER,
            Predeploys::L2_STANDARD_BRIDGE,
            Address::repeat_byte(4),
        );
        let log: alloy_rpc_types_eth::Log =
            serde_json::from_value(receipt["logs"][0].clone()).unwrap();
        let hash = MessagePassed::decode_log(&log.inner).unwrap().data.withdrawalHash;
        let unrelated = if let Some((sender, relay_sender, from)) = unrelated_senders {
            disbursal_receipt(sender, relay_sender, from)
        } else {
            serde_json::from_str(include_str!(
                "../../withdrawer/fixtures/synthetic-deposit-withdrawal-receipt.json"
            ))
            .unwrap()
        };
        receipt["logs"].as_array_mut().unwrap().push(unrelated["logs"][0].clone());
        let estimated = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&estimated);
        let published = Arc::new(Mutex::new(Vec::new()));
        let sent = Arc::clone(&published);
        let proof_reads = Arc::new(Mutex::new(0));
        let status_reads = Arc::new(Mutex::new(0));
        let captured_status_reads = Arc::clone(&status_reads);
        let captured_proof_reads = Arc::clone(&proof_reads);
        let estimation_started = Arc::new(Notify::new());
        let captured_estimation_started = Arc::clone(&estimation_started);
        let handler = move |request: Value| match request["method"].as_str().unwrap() {
            "eth_blockNumber" | "eth_gasPrice" | "eth_maxPriorityFeePerGas" => {
                json!({"result":"0x64"})
            }
            "eth_getLogs" => json!({"result":[fee_log(100, 0, B256::repeat_byte(1))]}),
            "eth_getTransactionReceipt" if request["params"][0] == json!(B256::repeat_byte(1)) => {
                json!({"result":receipt})
            }
            "eth_getTransactionReceipt" => {
                if finalization_error == Some("was signed") {
                    return json!({"error":{"code":3,"message":"receipt unavailable"}});
                }
                let mut mined = receipt.clone();
                mined["type"] = json!("0x0");
                mined["blockNumber"] = json!("0x1");
                mined["transactionHash"] = request["params"][0].clone();
                mined["logs"] = json!([]);
                if matches!(finalization_error, Some("target call failed" | "success")) {
                    let event = WithdrawalPortal::WithdrawalFinalized {
                        withdrawalHash: hash,
                        success: finalization_error == Some("success"),
                    }
                    .encode_log_data();
                    let mut log = receipt["logs"][0].clone();
                    log["address"] = json!(Address::repeat_byte(1));
                    log["transactionHash"] = request["params"][0].clone();
                    log["topics"] = json!(event.topics());
                    log["data"] = json!(event.data);
                    mined["logs"] = json!([log]);
                }
                json!({"result":mined})
            }
            "eth_sendRawTransaction" => {
                let raw: Bytes = serde_json::from_value(request["params"][0].clone()).unwrap();
                sent.lock().unwrap().push(alloy_primitives::keccak256(raw));
                json!({"result":B256::repeat_byte(7)})
            }
            "eth_getBlockByNumber" => {
                let mut block = Block::<Transaction>::default();
                block.header.inner.timestamp = now;
                block.header.inner.base_fee_per_gas = Some(100);
                block.header.hash = serde_json::from_value(receipt["blockHash"].clone()).unwrap();
                json!({"result":block})
            }
            "eth_chainId" => json!({"result":"0x1"}),
            "eth_getTransactionCount" => json!({"result":"0x0"}),
            "eth_estimateGas" => {
                let data: Bytes =
                    serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
                let call =
                    WithdrawalPortal::finalizeWithdrawalTransactionCall::abi_decode_validate(&data)
                        .unwrap();
                assert_eq!(
                    call.withdrawal.data,
                    MessagePassed::decode_log(&log.inner).unwrap().data.data
                );
                captured.store(true, Ordering::Relaxed);
                if finalization_error.is_some()
                    || pending == Some(0) && recheck == Some(WithdrawalStatus::Finalize)
                {
                    return json!({"result":"0x10000"});
                }
                json!({"error":{"code":3,"message":"execution reverted: https://user:password@secret.endpoint","data":"0xdeadbeef"}})
            }
            "eth_call" => {
                let data: Bytes =
                    serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
                macro_rules! reply {
                    ($call:ty, $($value:expr),* $(,)?) => {
                        if data[..4] == <$call>::SELECTOR {
                            return json!({"result":Bytes::from(<$call>::abi_encode_returns_tuple(&($($value,)*)))});
                        }
                    };
                }
                reply!(FaultProofPortal::disputeGameFactoryCall, Address::repeat_byte(2));
                reply!(FaultProofPortal::anchorStateRegistryCall, Address::repeat_byte(3));
                let estimated = captured.load(Ordering::Relaxed);
                if data[..4] == WithdrawalPortal::finalizedWithdrawalsCall::SELECTOR {
                    let call =
                        WithdrawalPortal::finalizedWithdrawalsCall::abi_decode(&data).unwrap();
                    assert_eq!(call.withdrawalHash, hash, "unrelated withdrawal was processed");
                    *captured_status_reads.lock().unwrap() += 1;
                    if estimated && recheck.is_none() {
                        return json!({"error":{"code":-32000,"message":"execution reverted: secret.endpoint"}});
                    }
                }
                if data[..4] == FaultProofPortal::provenWithdrawalsCall::SELECTOR {
                    let mut reads = proof_reads.lock().unwrap();
                    *reads += 1;
                }
                reply!(
                    WithdrawalPortal::finalizedWithdrawalsCall,
                    estimated && recheck == Some(WithdrawalStatus::Finalized)
                );
                reply!(
                    IAnchorStateRegistry::pausedCall,
                    paused || estimated && recheck == Some(WithdrawalStatus::Paused)
                );
                reply!(
                    FaultProofPortal::provenWithdrawalsCall,
                    Address::repeat_byte(8),
                    if estimated && recheck == Some(WithdrawalStatus::Prove) {
                        0u64
                    } else {
                        800u64
                    }
                );
                reply!(IAnchorStateRegistry::isGameBlacklistedCall, false);
                reply!(IAnchorStateRegistry::isGameRetiredCall, false);
                reply!(IAnchorStateRegistry::isGameProperCall, true);
                reply!(IAnchorStateRegistry::isGameRespectedCall, true);
                reply!(WithdrawalDisputeGame::statusCall, 2u8);
                reply!(FaultProofPortal::proofMaturityDelaySecondsCall, U256::from(100));
                reply!(WithdrawalDisputeGame::resolvedAtCall, 700u64);
                reply!(IAnchorStateRegistry::disputeGameFinalityDelaySecondsCall, U256::from(200));
                reply!(
                    IAnchorStateRegistry::isGameFinalizedCall,
                    now > 900
                        && !(estimated
                            && matches!(recheck, Some(WithdrawalStatus::NotReady { .. })))
                );
                reply!(FaultProofPortal::checkWithdrawalCall,);
                panic!("unexpected contract selector {data}");
            }
            method => panic!("unexpected RPC method {method}"),
        };
        let server = Server::new_async(move |request| {
            let hang = finalization_error == Some("deadline before broadcast")
                && request["method"] == "eth_estimateGas";
            let response = handler(request);
            let started = Arc::clone(&captured_estimation_started);
            async move {
                if hang {
                    started.notify_one();
                    return std::future::pending().await;
                }
                response
            }
        })
        .await;
        let mut job = job(&server.url);
        job.args.lookback = Duration::from_nanos(1);
        job.args.dry_run = finalization_error.is_none();
        if finalization_error == Some("deadline before broadcast") {
            // Live estimation avoids a competing dry-run preparation timeout.
            job.args.withdrawal_timeout = Duration::from_secs(1);
        }
        if finalization_error == Some("was signed") {
            job.args.withdrawal_timeout = Duration::from_secs(1);
            job.args.tx_manager.resubmission_timeout = Duration::from_millis(100);
            job.args.tx_manager.receipt_query_interval = Duration::from_millis(25);
        }
        let capture = LogCapture(Arc::new(Mutex::new(Vec::new())));
        let writer = capture.clone();
        let subscriber =
            tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        let withdrawal_timeout = job.args.withdrawal_timeout;
        let run = job.run().with_subscriber(subscriber);
        tokio::pin!(run);
        let report = if finalization_error == Some("deadline before broadcast") {
            tokio::select! {
                () = estimation_started.notified() => {},
                report = &mut run => panic!("estimation did not hang: {report:?}"),
            }
            tokio::time::pause();
            tokio::time::advance(withdrawal_timeout).await;
            tokio::time::timeout(Duration::from_secs(1), &mut run)
                .await
                .expect("run must return at the withdrawal deadline")
        } else {
            run.await
        }
        .unwrap();
        let published = published.lock().unwrap().clone();
        assert_eq!(
            !published.is_empty(),
            finalization_error.is_some() && finalization_error != Some("deadline before broadcast"),
            "dry runs must not publish"
        );
        assert!(report.receipt_failures.is_empty());
        assert_eq!(report.withdrawals.len(), 1);
        assert_eq!(
            (report.withdrawals[0].0, report.withdrawals[0].1),
            (hash, B256::repeat_byte(1))
        );
        let outcome = &report.withdrawals[0].2;
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("skipping withdrawal not sent by the FeeDisburser"));
        if let Some(pending) = pending {
            assert_eq!(usize::from(outcome.is_pending()), pending);
            if pending == 0 {
                assert!(matches!(
                    (recheck, outcome),
                    (Some(WithdrawalStatus::Finalized), ConfirmOutcome::AlreadyFinalized)
                        | (Some(WithdrawalStatus::Finalize), ConfirmOutcome::Finalized(_))
                ));
                if let ConfirmOutcome::Finalized(transaction) = outcome {
                    assert_eq!(transaction.is_some(), finalization_error.is_some());
                    assert_eq!(
                        *status_reads.lock().unwrap(),
                        1,
                        "must not recheck a signed finalization"
                    );
                }
            } else if now < 900 || matches!(recheck, Some(WithdrawalStatus::NotReady { .. })) {
                assert!(
                    matches!(outcome, ConfirmOutcome::Pending { timing: Some(ConfirmTiming { ready_at: 901, l1_timestamp, waiting_for_game: false }) } if *l1_timestamp == now),
                    "{outcome:?}"
                );
                let seconds_until_earliest_ready = if now < 900 { 2 } else { 0 };
                assert!(
                    logs.lines().any(|line| {
                        line.contains(
                            "withdrawal pending; game resolution may delay readiness further",
                        ) && line.contains(&format!("withdrawal_hash={hash}"))
                            && line.contains("ready_at=901")
                            && line.contains(&format!("l1_timestamp={now}"))
                            && line.contains(&format!(
                                "seconds_until_earliest_ready={seconds_until_earliest_ready}"
                            ))
                    }),
                    "missing pending timing log for {hash}: {logs}"
                );
            } else {
                assert!(matches!(outcome, ConfirmOutcome::Pending { timing: None }));
            }
            assert!(report.into_result().is_ok());
        } else {
            let ConfirmOutcome::Failed(error) = outcome else { panic!("{outcome:?}") };
            if let Some(message) = finalization_error {
                if message == "deadline before broadcast" {
                    assert!(
                        matches!(
                            error.downcast_ref::<WithdrawerError>(),
                            Some(WithdrawerError::Timeout(_))
                        ),
                        "{error:#}"
                    );
                    assert_eq!(*status_reads.lock().unwrap(), 1);
                    assert!(!format!("{error:#}").contains("could not recheck"));
                    let error = report.into_result().unwrap_err();
                    assert!(matches!(
                        error.downcast_ref::<WithdrawerError>(),
                        Some(WithdrawerError::Timeout(_))
                    ));
                    return;
                }
                if message == "was signed" {
                    let Some(SubmitError::MaybeIncluded { transaction_hashes, .. }) =
                        error.downcast_ref::<WithdrawerError>().and_then(|error| match error {
                            WithdrawerError::Submit(error) => Some(error.as_ref()),
                            _ => None,
                        })
                    else {
                        panic!("missing signed hashes: {error:#}");
                    };
                    assert!(published.len() >= 2, "withdrawal must fee-bump before its deadline");
                    assert!(published.iter().all(|hash| transaction_hashes.contains(hash)));
                    assert!(transaction_hashes.len() >= published.len());
                    assert!(transaction_hashes.len() <= published.len() + 1);
                }
                let message =
                    if message == "was signed" { "may have been broadcast" } else { message };
                assert!(format!("{error:#}").contains(message), "{error:#}");
                let Some(WithdrawerError::Submit(error)) = error.downcast_ref::<WithdrawerError>()
                else {
                    panic!("missing submission error");
                };
                let SubmitError::MaybeIncluded { transaction_hashes, .. } = error.as_ref() else {
                    panic!("published finalization must not be rechecked");
                };
                assert!(published.iter().all(|hash| transaction_hashes.contains(hash)));
                assert_eq!(
                    *status_reads.lock().unwrap(),
                    1,
                    "must not recheck a signed finalization"
                );
            } else if !paused {
                assert!(matches!(
                    error.downcast_ref::<WithdrawerError>(),
                    Some(WithdrawerError::Request(error))
                        if error.as_error_resp().is_some_and(|payload| payload.code == 3)
                ));
            }
            if finalization_error.is_none() {
                assert!(format!("{error:#}").contains(if paused {
                    "paused"
                } else {
                    "execution reverted"
                }));
            }
            assert_eq!(
                format!("{error:#}").contains("could not recheck withdrawal status"),
                recheck.is_none()
            );
            assert!(!format!("{error:#} {error:?}").contains("secret.endpoint"));
            if let Some(WithdrawerError::Submit(submit)) = error.downcast_ref::<WithdrawerError>() {
                let SubmitError::MaybeIncluded { transaction_hashes, .. } = submit.as_ref() else {
                    panic!("missing hashes")
                };
                let expected = transaction_hashes.clone();
                let error = report.into_result().unwrap_err();
                assert!(matches!(
                    error.downcast_ref::<WithdrawerError>(),
                    Some(WithdrawerError::Submit(submit))
                        if matches!(submit.as_ref(), SubmitError::MaybeIncluded { transaction_hashes, .. } if *transaction_hashes == expected)
                ));
            }
        }
        if now < 900 {
            assert_eq!(
                *captured_proof_reads.lock().unwrap(),
                1,
                "pending must reuse the status proof read"
            );
        } else if !paused
            && finalization_error.is_none()
            && recheck.is_some_and(|status| {
                matches!(status, WithdrawalStatus::NotReady { .. } | WithdrawalStatus::Prove)
            })
        {
            assert_eq!(
                *captured_proof_reads.lock().unwrap(),
                2,
                "each status must read the proof only once"
            );
            assert!(
                logs.lines().any(|line| {
                    (line.contains("withdrawal not ready")
                        || line.contains("withdrawal pending; a new proof is needed"))
                        && line.contains("error=")
                        && line.contains("execution reverted")
                }),
                "{logs}"
            );
        }
        assert!(!logs.contains("secret.endpoint"));
        assert_eq!(estimated.load(Ordering::Relaxed), !paused && now > 900);
    }

    #[test]
    fn report_accepts_completed_and_pending_withdrawals() {
        let hash = B256::repeat_byte(1);
        for outcome in [
            ConfirmOutcome::AlreadyFinalized,
            ConfirmOutcome::Finalized(Some(hash)),
            ConfirmOutcome::Finalized(None),
            ConfirmOutcome::Proven(Some(hash)),
            ConfirmOutcome::Proven(None),
            ConfirmOutcome::Pending { timing: None },
            ConfirmOutcome::Pending {
                timing: Some(ConfirmTiming {
                    ready_at: 901,
                    l1_timestamp: 899,
                    waiting_for_game: true,
                }),
            },
        ] {
            assert!(
                ConfirmReport {
                    receipt_failures: vec![],
                    withdrawals: vec![(hash, hash, outcome)],
                }
                .into_result()
                .is_ok()
            );
        }
        assert!(ConfirmReport::default().into_result().is_ok());
    }

    #[rstest]
    #[case::receipt(true)]
    #[case::withdrawal(false)]
    fn report_failure_preserves_typed_source(#[case] receipt: bool) {
        let hash = B256::repeat_byte(1);
        let source = WithdrawerError::MissingReceipt.into();
        let report = if receipt {
            ConfirmReport { receipt_failures: vec![(hash, source)], withdrawals: vec![] }
        } else {
            ConfirmReport {
                receipt_failures: vec![],
                withdrawals: vec![(hash, hash, ConfirmOutcome::Failed(source))],
            }
        };
        let error = report.into_result().unwrap_err();
        assert_eq!(error.to_string(), "1 withdrawals or disbursal receipts failed");
        assert!(matches!(
            error.downcast_ref::<WithdrawerError>(),
            Some(WithdrawerError::MissingReceipt)
        ));
    }

    #[tokio::test]
    async fn rejects_zero_portal_factory_before_scanning() {
        let server = Server::new(move |request| {
            assert_eq!(request["method"], "eth_call");
            let data: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
            if data.as_ref() == FaultProofPortal::disputeGameFactoryCall::SELECTOR {
                json!({"result": Bytes::from(FaultProofPortal::disputeGameFactoryCall::abi_encode_returns(&Address::ZERO))})
            } else {
                assert_eq!(data.as_ref(), FaultProofPortal::anchorStateRegistryCall::SELECTOR);
                json!({"result": Bytes::from(FaultProofPortal::anchorStateRegistryCall::abi_encode_returns(&Address::repeat_byte(3)))})
            }
        }).await;
        let error = job(&server.url).run().await.unwrap_err();
        assert!(error.to_string().contains("zero address"), "{error:#}");
    }

    #[rstest]
    #[case::protocol_window(1_000_000, Duration::from_secs(21 * 24 * 60 * 60), Duration::from_secs(2), 1_000_000, vec![92_800..=1_000_000])]
    #[case::round_up(100, Duration::from_secs(5), Duration::from_secs(2), 3, vec![97..=99, 100..=100])]
    #[case::subsecond(100, Duration::from_millis(5), Duration::from_millis(2), 3, vec![97..=99, 100..=100])]
    #[case::genesis_clamp(5, Duration::from_secs(20), Duration::from_secs(2), 4, vec![0..=3, 4..=5])]
    #[case::at_genesis(0, Duration::from_secs(5), Duration::from_secs(2), 4, vec![0..=0])]
    #[case::maximum_duration(5, Duration::MAX, Duration::from_nanos(1), 4, vec![0..=3, 4..=5])]
    #[case::maximum_head(u64::MAX, Duration::from_secs(1), Duration::from_secs(2), 10, vec![u64::MAX - 1..=u64::MAX])]
    fn duration_window_rounds_up_clamps_and_chunks(
        #[case] current: u64,
        #[case] lookback: Duration,
        #[case] block_time: Duration,
        #[case] chunk: u64,
        #[case] expected: Vec<RangeInclusive<u64>>,
    ) {
        let ranges = ConfirmWindow::ranges(current, lookback, block_time, chunk);
        assert_eq!(ranges.as_ref().ok(), Some(&expected), "{ranges:?}");
    }

    #[rstest]
    #[case::zero_lookback(Duration::ZERO, Duration::from_secs(2), 3000)]
    #[case::zero_block_time(Duration::from_secs(5), Duration::ZERO, 3000)]
    #[case::zero_chunk(Duration::from_secs(5), Duration::from_secs(2), 0)]
    fn rejects_zero_window_inputs(
        #[case] lookback: Duration,
        #[case] block_time: Duration,
        #[case] chunk: u64,
    ) {
        assert!(ConfirmWindow::ranges(100, lookback, block_time, chunk).is_err());
    }

    #[test]
    fn default_window_needs_303_log_queries() {
        let ranges = ConfirmWindow::ranges(
            2_000_000,
            Duration::from_secs(21 * 24 * 60 * 60),
            Duration::from_secs(2),
            3000,
        )
        .unwrap();
        assert_eq!(ranges.len(), 303);
        assert_eq!(ranges.first().unwrap().start(), &(2_000_000 - 907_200));
        assert_eq!(ranges.last().unwrap().end(), &2_000_000);
    }
}
