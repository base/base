use std::{
    collections::HashSet,
    ops::RangeInclusive,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_network::TxSigner;
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Filter;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolError, SolEvent, sol};
use base_withdrawer::{
    FaultProofPortal, Fees, Network, ProofSystem, Secret, SignerSource, Withdrawal,
    WithdrawalPortal, WithdrawalRpc, WithdrawalSigner, WithdrawalStatus, Withdrawer,
    WithdrawerConfig, WithdrawerError,
};
use eyre::{WrapErr, ensure};
use tokio::time::{sleep, timeout};
use tracing::{error, info};

use crate::{ConfirmArgs, FeeDisburser};

sol! {
    /// Portal finalization errors.
    interface PortalErrors {
        /// The registered proof is still maturing.
        error OptimismPortal_ProofNotOldEnough();
        /// The game's root claim is not currently valid.
        error OptimismPortal_InvalidRootClaim();
        /// The withdrawal has already been consumed.
        error OptimismPortal_AlreadyFinalized();
    }
}

/// Non-fatal portal reverts during finalization or readiness checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeRevert {
    /// The proof or its dispute game is not ready.
    Pending,
    /// Another execution already consumed the withdrawal.
    AlreadyFinalized,
}

impl FinalizeRevert {
    /// Classifies redacted RPC revert data, including wrapped submission failures.
    pub fn classify(error: &WithdrawerError) -> Option<Self> {
        let error = match error {
            WithdrawerError::Unconfirmed { source, .. } => return Self::classify(source),
            WithdrawerError::Request(error) => error,
            _ => return None,
        };
        let data: String =
            serde_json::from_str(error.as_error_resp()?.data.as_ref()?.get()).ok()?;
        let data: Bytes = data.parse().ok()?;
        let selector = data.get(..4)?;
        if selector == PortalErrors::OptimismPortal_ProofNotOldEnough::SELECTOR
            || selector == PortalErrors::OptimismPortal_InvalidRootClaim::SELECTOR
        {
            Some(Self::Pending)
        } else if selector == PortalErrors::OptimismPortal_AlreadyFinalized::SELECTOR {
            Some(Self::AlreadyFinalized)
        } else if alloy_sol_types::Revert::abi_decode(&data)
            .is_ok_and(|revert| revert.reason.contains("proven withdrawal has not matured yet"))
        {
            Some(Self::Pending)
        } else {
            None
        }
    }
}

/// Inclusive L2 log-query ranges.
#[derive(Debug)]
pub struct ConfirmWindow;

impl ConfirmWindow {
    /// Rejects oversized lookbacks and splits the entire window into inclusive chunks.
    pub fn ranges(current: u64, window: u64, chunk: u64) -> eyre::Result<Vec<RangeInclusive<u64>>> {
        eyre::ensure!(
            window < current,
            "number of blocks to query must be smaller than the current block"
        );
        eyre::ensure!(chunk > 0, "number of blocks per query must be positive");
        let mut start = current - window;
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

/// Confirms recent fee disbursals using one serialized L1 signer.
#[derive(Debug)]
pub struct ConfirmJob {
    /// Command configuration.
    pub args: ConfirmArgs,
}

impl ConfirmJob {
    /// Maximum time for a block-number, receipt, log-query, or proof-age read.
    pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
    /// Pause between log queries to limit RPC load.
    pub const QUERY_PAUSE: Duration = Duration::from_millis(300);

    /// Validates the command and constructs a custom fault-proof withdrawer configuration.
    pub fn config(&self) -> eyre::Result<WithdrawerConfig> {
        ensure!(
            [self.args.l1_rpc_url.scheme(), self.args.l2_rpc_url.scheme()]
                .into_iter()
                .all(|scheme| matches!(scheme, "http" | "https")),
            "RPC URLs must use HTTP or HTTPS"
        );
        ensure!(
            [
                self.args.optimism_portal_proxy_address,
                self.args.dispute_game_factory_address,
                self.args.anchor_state_registry_address,
                self.args.l2_fee_disburser_address
            ]
            .into_iter()
            .all(|address| address != Address::ZERO),
            "contract addresses must not be zero"
        );
        ensure!(self.args.num_blocks_per_query > 0, "number of blocks per query must be positive");
        ensure!(!self.args.withdrawal_timeout.is_zero(), "withdrawal timeout must be positive");
        let signer: PrivateKeySigner = self.args.job_runner_key.0.parse().map_err(|_| {
            eyre::eyre!("JOB_RUNNER_KEY must be a valid 32-byte secp256k1 key in hex")
        })?;
        Ok(WithdrawerConfig {
            l1_rpc_url: self.args.l1_rpc_url.clone(),
            network: Network {
                l2_rpc_url: self.args.l2_rpc_url.clone(),
                portal: self.args.optimism_portal_proxy_address,
                proofs: ProofSystem::FaultProofs {
                    dispute_game_factory: self.args.dispute_game_factory_address,
                    anchor_state_registry: self.args.anchor_state_registry_address,
                },
                l1_chain_id: None,
            },
            withdrawal: B256::ZERO,
            signer: SignerSource::PrivateKey(Secret(signer.to_bytes())),
            gas_limit: None,
            fees: Fees::Automatic,
            gas_multiplier: 1.0,
            max_gas_price: None,
            dry_run: self.args.dry_run,
            timeout: self.args.withdrawal_timeout,
            max_retries: 3,
            retry_delay: Duration::from_secs(2),
        })
    }

    /// Queries disbursal transactions in block/log order, deduplicating repeated hashes.
    pub async fn disbursals(&self, rpc: &WithdrawalRpc) -> eyre::Result<Vec<B256>> {
        let current = timeout(Self::READ_TIMEOUT, rpc.l2.get_block_number())
            .await
            .wrap_err("L2 block number read timed out")?
            .map_err(WithdrawerError::from)?;
        let ranges = ConfirmWindow::ranges(
            current,
            self.args.num_blocks_to_query,
            self.args.num_blocks_per_query,
        )?;
        let mut transactions = Vec::new();
        let mut seen = HashSet::new();
        for (index, range) in ranges.into_iter().enumerate() {
            if index > 0 {
                sleep(Self::QUERY_PAUSE).await;
            }
            info!(start = %range.start(), end = %range.end(), "querying fee disbursals");
            let filter = Filter::new()
                .address(self.args.l2_fee_disburser_address)
                .event_signature(FeeDisburser::FeesDisbursed::SIGNATURE_HASH)
                .from_block(*range.start())
                .to_block(*range.end());
            let mut logs = timeout(Self::READ_TIMEOUT, rpc.l2.get_logs(&filter))
                .await
                .wrap_err("fee disbursal log query timed out")?
                .map_err(WithdrawerError::from)?;
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

    /// Fetches and extracts every withdrawal in a disbursal receipt within a bounded read.
    pub async fn withdrawals(
        rpc: &WithdrawalRpc,
        transaction: B256,
    ) -> eyre::Result<Vec<Withdrawal>> {
        let receipt = timeout(Self::READ_TIMEOUT, rpc.l2.get_transaction_receipt(transaction))
            .await
            .wrap_err("L2 receipt read timed out")?
            .map_err(WithdrawerError::from)?
            .ok_or(WithdrawerError::MissingReceipt)?;
        Withdrawal::from_receipt(&receipt).map_err(WithdrawerError::from).map_err(Into::into)
    }

    /// Logs the onchain proof age against the local clock, or the absence of a mined proof.
    pub async fn log_pending(
        &self,
        rpc: &WithdrawalRpc,
        hash: B256,
        submitter: Address,
    ) -> eyre::Result<()> {
        let proof = timeout(
            Self::READ_TIMEOUT,
            Withdrawer::read(
                rpc,
                self.args.optimism_portal_proxy_address,
                FaultProofPortal::provenWithdrawalsCall { withdrawalHash: hash, submitter },
            ),
        )
        .await
        .wrap_err("proof timestamp read timed out")??;
        if proof.timestamp == 0 {
            info!(withdrawal_hash = %hash, "withdrawal pending, just proven");
        } else {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            info!(withdrawal_hash = %hash, proof_timestamp = proof.timestamp,
                proof_age_seconds = now.saturating_sub(proof.timestamp), "withdrawal pending");
        }
        Ok(())
    }

    /// Handles readiness/finalization reverts; returns whether the withdrawal is pending.
    pub async fn finalize_error(
        &self,
        rpc: &WithdrawalRpc,
        hash: B256,
        submitter: Address,
        error: WithdrawerError,
    ) -> eyre::Result<bool> {
        match FinalizeRevert::classify(&error) {
            Some(FinalizeRevert::Pending) => {
                info!(withdrawal_hash = %hash, error = %format_args!("{error:#}"), "withdrawal not ready");
                self.log_pending(rpc, hash, submitter).await?;
                Ok(true)
            }
            Some(FinalizeRevert::AlreadyFinalized) => {
                info!(withdrawal_hash = %hash, "withdrawal already finalized");
                Ok(false)
            }
            None => Err(error.into()),
        }
    }

    /// Processes one withdrawal; prove failures are terminal, while readiness reverts can be pending.
    pub async fn confirm_withdrawal(
        &self,
        withdrawer: &Withdrawer,
        rpc: &WithdrawalRpc,
        signer: &WithdrawalSigner,
        withdrawal: &Withdrawal,
        pending_transaction: &mut Option<B256>,
    ) -> eyre::Result<bool> {
        let hash = withdrawal.hash();
        let submitter = signer.address();
        let status = match withdrawer.status(rpc, withdrawal, submitter).await {
            Ok(status) => status,
            Err(error) => return self.finalize_error(rpc, hash, submitter, error).await,
        };
        info!(withdrawal_hash = %hash, ?status, "withdrawal decision");
        match status {
            WithdrawalStatus::Finalized => {
                info!(withdrawal_hash = %hash, "withdrawal already finalized");
                Ok(false)
            }
            WithdrawalStatus::Paused => {
                eyre::bail!("withdrawal portal or anchor state registry is paused")
            }
            WithdrawalStatus::NotReady { .. } => {
                self.log_pending(rpc, hash, submitter).await?;
                Ok(true)
            }
            WithdrawalStatus::Prove => {
                let calldata = withdrawer.prove_calldata(rpc, withdrawal).await?;
                withdrawer.submit(rpc, signer, calldata, None, pending_transaction).await?;
                if self.args.dry_run {
                    info!(withdrawal_hash = %hash, "dry run proof prepared");
                } else {
                    info!(withdrawal_hash = %hash, "withdrawal pending, just proven");
                }
                Ok(true)
            }
            WithdrawalStatus::Finalize => {
                let calldata = WithdrawalPortal::finalizeWithdrawalTransactionCall {
                    withdrawal: withdrawal.transaction.clone(),
                }
                .abi_encode()
                .into();
                match withdrawer
                    .submit(rpc, signer, calldata, Some(hash), pending_transaction)
                    .await
                {
                    Ok(transaction_hash) => {
                        if self.args.dry_run {
                            info!(withdrawal_hash = %hash, "dry run finalization prepared");
                        } else {
                            info!(withdrawal_hash = %hash, ?transaction_hash, "withdrawal finalized");
                        }
                        Ok(false)
                    }
                    Err(error) => self.finalize_error(rpc, hash, submitter, error).await,
                }
            }
        }
    }

    /// Processes all receipts and withdrawals, returning failure only after collecting all failures.
    pub async fn run(self) -> eyre::Result<()> {
        let config = self.config()?;
        let rpc = WithdrawalRpc::new(&config).map_err(WithdrawerError::from)?;
        let signer = WithdrawalSigner::new(&config.signer).await.map_err(WithdrawerError::from)?;
        let withdrawer = Withdrawer::new(config);
        let transactions = self.disbursals(&rpc).await?;
        let mut pending = Vec::new();
        let mut failed = Vec::new();
        let mut last_error = None;
        for transaction in transactions {
            let withdrawals = match Self::withdrawals(&rpc, transaction).await {
                Ok(withdrawals) => withdrawals,
                Err(error) => {
                    error!(transaction_hash = %transaction, error = %format_args!("{error:#}"), "could not extract disbursal withdrawals");
                    failed.push(transaction);
                    last_error = Some(error);
                    continue;
                }
            };
            for withdrawal in withdrawals {
                let hash = withdrawal.hash();
                let mut pending_transaction = None;
                let result = timeout(
                    self.args.withdrawal_timeout,
                    self.confirm_withdrawal(
                        &withdrawer,
                        &rpc,
                        &signer,
                        &withdrawal,
                        &mut pending_transaction,
                    ),
                )
                .await
                .unwrap_or_else(|source| {
                    Err(match pending_transaction {
                        Some(transaction_hash) => WithdrawerError::Unconfirmed {
                            transaction_hash,
                            source: Box::new(WithdrawerError::Timeout(source)),
                        },
                        None => WithdrawerError::Timeout(source),
                    }
                    .into())
                });
                match result {
                    Ok(true) => pending.push(hash),
                    Ok(false) => {}
                    Err(error) => {
                        error!(withdrawal_hash = %hash, transaction_hash = %transaction, error = %format_args!("{error:#}"), "withdrawal failed");
                        failed.push(hash);
                        last_error = Some(error);
                    }
                }
            }
        }
        info!(count = pending.len(), hashes = ?pending, "pending withdrawals");
        info!(count = failed.len(), hashes = ?failed, "failed withdrawals or disbursal receipts");
        if let Some(error) = last_error {
            return Err(error)
                .wrap_err(format!("{} withdrawals or disbursal receipts failed", failed.len()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use alloy_json_rpc::ErrorPayload;
    use alloy_primitives::U256;
    use alloy_transport::TransportError;
    use axum::{Json, Router, routing::post};
    use base_withdrawer::{MessagePassed, WithdrawalTransaction};
    use rstest::rstest;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};
    use url::Url;

    use super::*;

    struct Server {
        url: Url,
        task: JoinHandle<()>,
    }

    impl Server {
        async fn new(handler: impl Fn(Value) -> Value + Send + Sync + 'static) -> Self {
            let handler = Arc::new(handler);
            let router = Router::new().fallback(post(move |Json(request): Json<Value>| {
                let mut response = handler(request.clone());
                response["jsonrpc"] = json!("2.0");
                response["id"] = request["id"].clone();
                async move { Json(response) }
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
                l1_rpc_url: url.clone(),
                l2_rpc_url: url.clone(),
                optimism_portal_proxy_address: Address::repeat_byte(1),
                dispute_game_factory_address: Address::repeat_byte(2),
                anchor_state_registry_address: Address::repeat_byte(3),
                l2_fee_disburser_address: Address::repeat_byte(4),
                job_runner_key: Secret("01".repeat(32)),
                num_blocks_to_query: 9,
                num_blocks_per_query: 4,
                dry_run: true,
                withdrawal_timeout: Duration::from_secs(10),
            },
        }
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
        let mut receipt: Value = serde_json::from_str(include_str!(
            "../../withdrawer/fixtures/synthetic-deposit-withdrawal-receipt.json"
        ))
        .unwrap();
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
        let visited = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&visited);
        let server = Server::new(move |request| match request["method"].as_str().unwrap() {
            "eth_blockNumber" => json!({"result":"0x64"}),
            "eth_getLogs" => json!({"result":[fee_log(100, 0, B256::repeat_byte(1)), fee_log(100, 1, B256::repeat_byte(2))]}),
            "eth_getTransactionReceipt" if request["params"][0] == json!(B256::repeat_byte(1)) => json!({"result":null}),
            "eth_getTransactionReceipt" => json!({"result":receipt}),
            "eth_call" => {
                let data: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
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
        job.args.num_blocks_to_query = 0;
        let error = job.run().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("2 withdrawals or disbursal receipts failed"),
            "{error:#}"
        );
        assert!(!format!("{error:#} {error:?}").contains("secret.endpoint"));
        assert_eq!(*visited.lock().unwrap(), vec![event.withdrawalHash, second.hash()]);
    }

    fn revert(data: Option<Bytes>) -> WithdrawerError {
        let mut payload = json!({"code":3,"message":"execution reverted: secret endpoint"});
        if let Some(data) = data {
            payload["data"] = json!(data);
        }
        WithdrawerError::from(TransportError::ErrorResp(
            serde_json::from_value::<ErrorPayload>(payload).unwrap(),
        ))
    }

    #[rstest]
    #[case::proof_age("0xd9bc01be".parse().unwrap(), Some(FinalizeRevert::Pending))]
    #[case::root_claim("0x332a57f8".parse().unwrap(), Some(FinalizeRevert::Pending))]
    #[case::finalized("0x730a1074".parse().unwrap(), Some(FinalizeRevert::AlreadyFinalized))]
    #[case::legacy(alloy_sol_types::Revert::from("OptimismPortal: proven withdrawal has not matured yet").abi_encode().into(), Some(FinalizeRevert::Pending))]
    #[case::unrelated("0xdeadbeef".parse().unwrap(), None)]
    fn classifies_redacted_portal_reverts(
        #[case] data: Bytes,
        #[case] expected: Option<FinalizeRevert>,
    ) {
        assert_eq!(FinalizeRevert::classify(&revert(Some(data))), expected);
    }

    #[test]
    fn revert_without_data_is_a_failure() {
        assert_eq!(FinalizeRevert::classify(&revert(None)), None);
    }

    #[test]
    fn unconfirmed_preserves_revert_classification() {
        let error = WithdrawerError::Unconfirmed {
            transaction_hash: Default::default(),
            source: Box::new(revert(Some("0x730a1074".parse().unwrap()))),
        };
        assert_eq!(FinalizeRevert::classify(&error), Some(FinalizeRevert::AlreadyFinalized));
    }

    #[rstest]
    #[case::equal(100)]
    #[case::larger(101)]
    fn rejects_window_at_or_above_head(#[case] window: u64) {
        assert!(
            ConfirmWindow::ranges(100, window, 3).is_err(),
            "window >= current block must fail"
        );
    }

    #[test]
    fn chunks_are_inclusive_contiguous_and_truncated_at_head() {
        assert_eq!(ConfirmWindow::ranges(100, 9, 4).unwrap(), vec![91..=94, 95..=98, 99..=100]);
        assert_eq!(ConfirmWindow::ranges(100, 0, 4).unwrap(), vec![100..=100]);
        assert_eq!(ConfirmWindow::ranges(u64::MAX, 1, 10).unwrap(), vec![u64::MAX - 1..=u64::MAX]);
        assert!(ConfirmWindow::ranges(100, 1, 0).is_err());
    }
}
