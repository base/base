use std::{
    collections::HashSet,
    ops::RangeInclusive,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_network::TxSigner;
use alloy_primitives::{Address, B256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::Filter;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent};
use base_common_consensus::Predeploys;
use base_tx_manager::SimpleTxManager;
use base_withdrawer::{
    BroadcastJournal, FaultProofContracts, FaultProofPortal, Network, ProofSystem,
    ResolvedProofSystem, Secret, SignerSource, Withdrawal, WithdrawalPortal, WithdrawalRpc,
    WithdrawalSigner, WithdrawalStatus, Withdrawer, WithdrawerConfig, WithdrawerError,
};
use eyre::{WrapErr, ensure};
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};

use crate::{ConfirmArgs, CrossDomainMessenger, FeeDisburser, StandardBridge};

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

/// L1 submission identity and the manager, absent for dry runs.
#[derive(Debug)]
pub struct ConfirmSender<'a> {
    /// Address used for proof selection and gas estimation.
    pub address: Address,
    /// Shared manager for serialized L1 sends.
    pub manager: Option<&'a SimpleTxManager<RootProvider>>,
}

/// Confirms recent fee disbursals using one serialized L1 signer.
#[derive(Debug)]
pub struct ConfirmJob {
    /// Command configuration.
    pub args: ConfirmArgs,
}

impl ConfirmJob {
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
            [self.args.optimism_portal_proxy_address, self.args.l2_fee_disburser_address]
                .into_iter()
                .chain(self.args.dispute_game_factory_address)
                .chain(self.args.anchor_state_registry_address)
                .all(|address| address != Address::ZERO),
            "contract addresses must not be zero"
        );
        let expected =
            match (self.args.dispute_game_factory_address, self.args.anchor_state_registry_address)
            {
                (Some(dispute_game_factory), Some(anchor_state_registry)) => {
                    Some(FaultProofContracts { dispute_game_factory, anchor_state_registry })
                }
                (None, None) => None,
                _ => eyre::bail!(
                    "dispute game factory and anchor state registry must be supplied together"
                ),
            };
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
                proofs: ProofSystem::FaultProofs { expected },
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
        })
    }

    /// Queries disbursal transactions in block/log order, deduplicating repeated hashes.
    pub async fn disbursals(&self, rpc: &WithdrawalRpc) -> eyre::Result<Vec<B256>> {
        let current = rpc.l2.get_block_number().await.map_err(WithdrawerError::from)?;
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

    /// Logs the onchain proof age against the local clock, or the absence of a mined proof.
    pub async fn log_pending(&self, rpc: &WithdrawalRpc, hash: B256, submitter: Address) {
        let proof = match Withdrawer::read(
            rpc,
            self.args.optimism_portal_proxy_address,
            FaultProofPortal::provenWithdrawalsCall { withdrawalHash: hash, submitter },
        )
        .await
        {
            Ok(proof) => proof,
            Err(error) => {
                warn!(withdrawal_hash = %hash, error = %format_args!("{error:#}"), "could not read pending withdrawal proof age");
                return;
            }
        };
        if proof.timestamp == 0 {
            info!(withdrawal_hash = %hash, "withdrawal pending, just proven");
        } else {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            info!(withdrawal_hash = %hash, proof_timestamp = proof.timestamp,
                proof_age_seconds = now.saturating_sub(proof.timestamp), "withdrawal pending");
        }
    }

    /// Processes one withdrawal without hiding signed finalization failures.
    pub async fn confirm_withdrawal(
        &self,
        withdrawer: &Withdrawer,
        rpc: &WithdrawalRpc,
        proofs: ResolvedProofSystem,
        sender: &ConfirmSender<'_>,
        withdrawal: &Withdrawal,
        pending_transaction: &BroadcastJournal,
    ) -> eyre::Result<bool> {
        let hash = withdrawal.hash();
        let submitter = sender.address;
        let status = withdrawer.status(rpc, proofs, withdrawal, submitter).await?;
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
                self.log_pending(rpc, hash, submitter).await;
                Ok(true)
            }
            WithdrawalStatus::Prove => {
                let calldata = withdrawer.prove_calldata(rpc, proofs, withdrawal).await?;
                withdrawer
                    .submit(rpc, submitter, sender.manager, calldata, None, pending_transaction)
                    .await?;
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
                    .submit(
                        rpc,
                        submitter,
                        sender.manager,
                        calldata,
                        Some(hash),
                        pending_transaction,
                    )
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
                    Err(
                        error @ (WithdrawerError::TargetCallFailed { .. }
                        | WithdrawerError::MissingFinalizationEvent { .. }
                        | WithdrawerError::Unconfirmed { .. }),
                    ) => Err(error.into()),
                    Err(error) => match withdrawer.status(rpc, proofs, withdrawal, submitter).await
                    {
                        Ok(WithdrawalStatus::Finalized) => {
                            info!(withdrawal_hash = %hash, "withdrawal already finalized");
                            Ok(false)
                        }
                        Ok(WithdrawalStatus::NotReady { .. } | WithdrawalStatus::Prove) => {
                            info!(withdrawal_hash = %hash, error = %format_args!("{error:#}"), "withdrawal not ready");
                            self.log_pending(rpc, hash, submitter).await;
                            Ok(true)
                        }
                        Ok(_) => Err(error.into()),
                        Err(recheck_error) => Err(error).wrap_err(format!(
                            "could not recheck withdrawal status: {recheck_error:#}"
                        )),
                    },
                }
            }
        }
    }

    /// Processes all receipts and withdrawals, returning failure only after collecting all failures.
    pub async fn run(self) -> eyre::Result<()> {
        let config = self.config()?;
        let rpc = WithdrawalRpc::new(&config).map_err(WithdrawerError::from)?;
        let withdrawer = Withdrawer::new(config);
        let proofs = withdrawer.resolve_proof_system(&rpc).await?;
        let signer = WithdrawalSigner::new(&withdrawer.config.signer)
            .await
            .map_err(WithdrawerError::from)?;
        let submitter = signer.address();
        let transactions = self.disbursals(&rpc).await?;
        let pending_transaction = BroadcastJournal::default();
        let manager = if self.args.dry_run {
            None
        } else {
            Some(withdrawer.manager(signer, pending_transaction.clone()).await?)
        };
        let mut pending = Vec::new();
        let sender = ConfirmSender { address: submitter, manager: manager.as_ref() };
        let mut failed = Vec::new();
        let mut last_error = None;
        for transaction in transactions {
            let withdrawals = match self.withdrawals(&rpc, transaction).await {
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
                pending_transaction.clear();
                let result = timeout(
                    self.args.withdrawal_timeout,
                    self.confirm_withdrawal(
                        &withdrawer,
                        &rpc,
                        proofs,
                        &sender,
                        &withdrawal,
                        &pending_transaction,
                    ),
                )
                .await
                .unwrap_or_else(|source| {
                    let transaction_hashes = pending_transaction.hashes();
                    Err(if transaction_hashes.is_empty() {
                        WithdrawerError::Timeout(source)
                    } else {
                        WithdrawerError::Unconfirmed {
                            transaction_hashes,
                            source: Box::new(WithdrawerError::Timeout(source)),
                        }
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
        if let Some(manager) = manager {
            manager.close();
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
    use std::{
        io::{self, Write},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use alloy_primitives::{Bytes, U256};
    use alloy_rpc_types_eth::{Block, Transaction};
    use axum::{Json, Router, routing::post};
    use base_proof_contracts::IAnchorStateRegistry;
    use base_withdrawer::{MessagePassed, WithdrawalDisputeGame, WithdrawalTransaction};
    use rstest::rstest;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};
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
                tx_manager: crate::TxManagerCli { num_confirmations: 1, ..Default::default() },
                l1_rpc_url: url.clone(),
                l2_rpc_url: url.clone(),
                optimism_portal_proxy_address: Address::repeat_byte(1),
                dispute_game_factory_address: Some(Address::repeat_byte(2)),
                anchor_state_registry_address: Some(Address::repeat_byte(3)),
                l2_fee_disburser_address: Address::repeat_byte(4),
                job_runner_key: Secret("01".repeat(32)),
                num_blocks_to_query: 9,
                num_blocks_per_query: 4,
                dry_run: true,
                withdrawal_timeout: Duration::from_secs(10),
            },
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

    #[rstest]
    #[case::discovered(false)]
    #[case::expected(true)]
    #[tokio::test]
    async fn continues_after_failed_receipts_and_withdrawals_and_returns_failure(
        #[case] expected: bool,
    ) {
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
        if !expected {
            job.args.dispute_game_factory_address = None;
            job.args.anchor_state_registry_address = None;
        }
        job.args.num_blocks_to_query = 0;
        let error = job.run().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("3 withdrawals or disbursal receipts failed"),
            "{error:#}"
        );
        assert!(!format!("{error:#} {error:?}").contains("secret.endpoint"));
        assert_eq!(*visited.lock().unwrap(), vec![event.withdrawalHash, second.hash()]);
    }

    #[rstest]
    #[case::pending(900, false, Some(WithdrawalStatus::Finalize), false, Some(1), None)]
    #[case::pending_proof_age_read_fails(
        900,
        false,
        Some(WithdrawalStatus::Finalize),
        true,
        Some(1),
        None
    )]
    #[case::paused(1000, true, Some(WithdrawalStatus::Finalize), false, None, None)]
    #[case::already_finalized_during_estimation(
        1000,
        false,
        Some(WithdrawalStatus::Finalized),
        false,
        Some(0),
        None
    )]
    #[case::not_ready_during_estimation(1000, false, Some(WithdrawalStatus::NotReady { ready_at: 900, waiting_for_game: false }), false, Some(1), None)]
    #[case::proof_invalidated_during_estimation(
        1000,
        false,
        Some(WithdrawalStatus::Prove),
        false,
        Some(1),
        None
    )]
    #[case::still_ready_during_estimation(
        1000,
        false,
        Some(WithdrawalStatus::Finalize),
        false,
        None,
        None
    )]
    #[case::paused_during_estimation(
        1000,
        false,
        Some(WithdrawalStatus::Paused),
        false,
        None,
        None
    )]
    #[case::recheck_read_fails(1000, false, None, false, None, None)]
    #[case::target_call_failed(
        1000,
        false,
        Some(WithdrawalStatus::Finalized),
        false,
        None,
        Some("target call failed")
    )]
    #[case::missing_event(
        1000,
        false,
        Some(WithdrawalStatus::Finalized),
        false,
        None,
        Some("no portal finalization event")
    )]
    #[case::unconfirmed(
        1000,
        false,
        Some(WithdrawalStatus::Finalized),
        false,
        None,
        Some("was signed")
    )]
    #[tokio::test]
    async fn run_handles_pending_paused_races_and_unrelated_withdrawals(
        #[case] now: u64,
        #[case] paused: bool,
        #[case] recheck: Option<WithdrawalStatus>,
        #[case] proof_age_fails: bool,
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
        let server = Server::new(move |request| match request["method"].as_str().unwrap() {
            "eth_blockNumber" | "eth_gasPrice" | "eth_maxPriorityFeePerGas" => json!({"result":"0x64"}),
            "eth_getLogs" => json!({"result":[fee_log(100, 0, B256::repeat_byte(1))]}),
            "eth_getTransactionReceipt" if request["params"][0] == json!(B256::repeat_byte(1)) => json!({"result":receipt}),
            "eth_getTransactionReceipt" => {
                if finalization_error == Some("was signed") {
                    return json!({"error":{"code":3,"message":"receipt unavailable"}});
                }
                let mut mined = receipt.clone();
                mined["type"] = json!("0x0");
                mined["blockNumber"] = json!("0x1");
                mined["transactionHash"] = request["params"][0].clone();
                mined["logs"] = json!([]);
                if finalization_error == Some("target call failed") {
                    let event = WithdrawalPortal::WithdrawalFinalized {
                        withdrawalHash: hash,
                        success: false,
                    }.encode_log_data();
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
                let data: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
                let call = WithdrawalPortal::finalizeWithdrawalTransactionCall::abi_decode_validate(&data).unwrap();
                assert_eq!(call.withdrawal.data, MessagePassed::decode_log(&log.inner).unwrap().data.data);
                captured.store(true, Ordering::Relaxed);
                if finalization_error.is_some() {
                    return json!({"result":"0x10000"});
                }
                json!({"error":{"code":3,"message":"execution reverted: https://user:password@secret.endpoint","data":"0xdeadbeef"}})
            }
            "eth_call" => {
                let data: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
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
                    let call = WithdrawalPortal::finalizedWithdrawalsCall::abi_decode(&data).unwrap();
                    assert_eq!(call.withdrawalHash, hash, "unrelated withdrawal was processed");
                    *captured_status_reads.lock().unwrap() += 1;
                    if estimated && recheck.is_none() {
                        return json!({"error":{"code":-32000,"message":"execution reverted: secret.endpoint"}});
                    }
                }
                if data[..4] == FaultProofPortal::provenWithdrawalsCall::SELECTOR {
                    let mut reads = proof_reads.lock().unwrap();
                    *reads += 1;
                    if proof_age_fails && *reads > 1 {
                        return json!({"error":{"code":3,"message":"execution reverted: secret.endpoint"}});
                    }
                }
                reply!(WithdrawalPortal::finalizedWithdrawalsCall, estimated && recheck == Some(WithdrawalStatus::Finalized));
                reply!(IAnchorStateRegistry::pausedCall, paused || estimated && recheck == Some(WithdrawalStatus::Paused));
                reply!(FaultProofPortal::provenWithdrawalsCall, Address::repeat_byte(8), if estimated && recheck == Some(WithdrawalStatus::Prove) { 0u64 } else { 800u64 });
                reply!(IAnchorStateRegistry::isGameBlacklistedCall, false);
                reply!(IAnchorStateRegistry::isGameRetiredCall, false);
                reply!(IAnchorStateRegistry::isGameProperCall, true);
                reply!(IAnchorStateRegistry::isGameRespectedCall, true);
                reply!(WithdrawalDisputeGame::statusCall, 2u8);
                reply!(FaultProofPortal::proofMaturityDelaySecondsCall, U256::from(100));
                reply!(WithdrawalDisputeGame::resolvedAtCall, 700u64);
                reply!(IAnchorStateRegistry::disputeGameFinalityDelaySecondsCall, U256::from(200));
                reply!(IAnchorStateRegistry::isGameFinalizedCall, now > 900 && !(estimated && matches!(recheck, Some(WithdrawalStatus::NotReady { .. }))));
                reply!(FaultProofPortal::checkWithdrawalCall,);
                panic!("unexpected contract selector {data}");
            }
            method => panic!("unexpected RPC method {method}"),
        }).await;
        let mut job = job(&server.url);
        job.args.num_blocks_to_query = 0;
        job.args.dry_run = finalization_error.is_none();
        if finalization_error == Some("was signed") {
            job.args.withdrawal_timeout = Duration::from_secs(1);
            job.args.tx_manager.resubmission_timeout = Duration::from_millis(100);
            job.args.tx_manager.receipt_query_interval = Duration::from_millis(25);
        }
        let capture = LogCapture(Arc::new(Mutex::new(Vec::new())));
        let writer = capture.clone();
        let subscriber =
            tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        let result = job.run().with_subscriber(subscriber).await;
        let published = published.lock().unwrap().clone();
        assert_eq!(
            !published.is_empty(),
            finalization_error.is_some(),
            "dry runs must not publish"
        );
        assert_eq!(result.is_ok(), pending.is_some(), "{result:?}");
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("skipping withdrawal not sent by the FeeDisburser"));
        if let Some(pending) = pending {
            assert!(logs.contains(&format!("pending withdrawals count={pending}")), "{logs}");
            assert!(logs.contains("failed withdrawals or disbursal receipts count=0"), "{logs}");
        } else {
            let error = result.unwrap_err();
            if let Some(message) = finalization_error {
                if message == "was signed" {
                    let Some(WithdrawerError::Unconfirmed { transaction_hashes, .. }) =
                        error.downcast_ref::<WithdrawerError>()
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
                assert!(matches!(
                    error.downcast_ref::<WithdrawerError>(),
                    Some(
                        WithdrawerError::TargetCallFailed { .. }
                            | WithdrawerError::MissingFinalizationEvent { .. }
                            | WithdrawerError::Unconfirmed { .. }
                    )
                ));
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
        }
        if proof_age_fails {
            assert!(logs.contains("could not read pending withdrawal proof age"), "{logs}");
        }
        assert!(!logs.contains("secret.endpoint"));
        assert_eq!(estimated.load(Ordering::Relaxed), !paused && now > 900);
    }

    #[rstest]
    #[case::mismatch(Address::repeat_byte(5), "do not match")]
    #[case::zero(Address::ZERO, "zero address")]
    #[tokio::test]
    async fn rejects_invalid_portal_contracts_before_scanning(
        #[case] factory: Address,
        #[case] message: &str,
    ) {
        let server = Server::new(move |request| {
            assert_eq!(request["method"], "eth_call");
            let data: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
            if data.as_ref() == FaultProofPortal::disputeGameFactoryCall::SELECTOR {
                json!({"result": Bytes::from(FaultProofPortal::disputeGameFactoryCall::abi_encode_returns(&factory))})
            } else {
                assert_eq!(data.as_ref(), FaultProofPortal::anchorStateRegistryCall::SELECTOR);
                json!({"result": Bytes::from(FaultProofPortal::anchorStateRegistryCall::abi_encode_returns(&Address::repeat_byte(3)))})
            }
        }).await;
        let error = job(&server.url).run().await.unwrap_err();
        assert!(error.to_string().contains(message), "{error:#}");
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
