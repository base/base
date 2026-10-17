use alloy_consensus::{Transaction, TxEip1559, TxEnvelope, TxLegacy, TypedTransaction};
use alloy_eips::BlockNumberOrTag;
use alloy_network::TxSigner;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_transport::TransportError;
use thiserror::Error;

use crate::{Fees, WithdrawalRpc, WithdrawalRpcTransport, WithdrawalSigner, WithdrawerConfig};

/// An unsigned L1 call and the sender used for estimation.
#[derive(Debug, Clone)]
pub struct PreparedWithdrawalTransaction {
    /// Sender whose pending nonce and balance are used.
    sender: Address,
    /// Unsigned transaction, available without a signer for dry runs.
    transaction: TypedTransaction,
}

/// L1 transaction preparation or signing failed.
#[derive(Debug, Error)]
pub enum WithdrawalTransactionError {
    /// A provider request failed after credential-bearing transport details were removed.
    #[error("transaction preparation RPC request failed: {0}")]
    Request(#[source] TransportError),
    /// The overall preparation deadline elapsed.
    #[error("transaction preparation timed out")]
    Timeout(#[source] tokio::time::error::Elapsed),
    /// The latest block is unavailable for automatic fee selection.
    #[error("latest L1 block is unavailable")]
    MissingLatestBlock,
    /// The L1 RPC serves a different chain than the selected network's contracts.
    #[error("L1 RPC reports chain ID {actual}, but the network expects {expected}")]
    L1ChainMismatch {
        /// Chain ID the network's contracts live on.
        expected: u64,
        /// Chain ID reported by the L1 RPC.
        actual: u64,
    },
    /// A directly constructed configuration has an invalid multiplier.
    #[error("gas multiplier must be finite and at least 1.0")]
    InvalidGasMultiplier,
    /// A final per-gas fee exceeds the configured cap.
    #[error("transaction fee {fee} exceeds max gas price {cap}")]
    FeeCap {
        /// Rejected fee in wei per gas.
        fee: u128,
        /// Configured cap in wei per gas.
        cap: u128,
    },
    /// The multiplied gas estimate does not fit in a transaction.
    #[error("multiplied gas estimate does not fit u64")]
    GasLimitOverflow,
    /// The signer differs from the sender used to prepare the call.
    #[error("signer {signer} does not match prepared sender {sender}")]
    SignerMismatch {
        /// Sender used during preparation.
        sender: Address,
        /// Address of the supplied signer.
        signer: Address,
    },
    /// Signing failed, including device policy checks.
    #[error("transaction signing failed: {0}")]
    Signing(#[source] alloy_signer::Error),
}

impl From<TransportError> for WithdrawalTransactionError {
    fn from(error: TransportError) -> Self {
        Self::Request(WithdrawalRpcTransport::redact(error))
    }
}

impl PreparedWithdrawalTransaction {
    /// Sender used to fetch the pending nonce and estimate gas.
    pub const fn sender(&self) -> Address {
        self.sender
    }

    /// Unsigned call, read-only to preserve the fee cap and estimation sender until signing.
    pub const fn transaction(&self) -> &TypedTransaction {
        &self.transaction
    }

    /// Prepares an L1 call without accessing signing material.
    ///
    /// Fetches a fresh pending nonce to include transactions already in the mempool.
    /// This does not reserve a nonce; callers must serialize preparation and submission
    /// for a sender. The complete preparation is bounded by `rpc.timeout`.
    pub async fn prepare(
        config: &WithdrawerConfig,
        rpc: &WithdrawalRpc,
        sender: Address,
        target: Address,
        input: Bytes,
    ) -> Result<Self, WithdrawalTransactionError> {
        tokio::time::timeout(rpc.timeout, async {
            let chain_id = rpc.l1.get_chain_id().await?;
            if let Some(expected) = config.network.l1_chain_id
                && expected != chain_id
            {
                return Err(WithdrawalTransactionError::L1ChainMismatch {
                    expected,
                    actual: chain_id,
                });
            }
            let nonce = rpc.l1.get_transaction_count(sender).pending().await?;
            let fees = match config.fees {
                Fees::Automatic => {
                    let block = rpc
                        .l1
                        .get_block_by_number(BlockNumberOrTag::Latest)
                        .await?
                        .ok_or(WithdrawalTransactionError::MissingLatestBlock)?;
                    if block.header.base_fee_per_gas.is_some() {
                        let estimate = rpc.l1.estimate_eip1559_fees().await?;
                        Fees::Eip1559 {
                            max_fee_per_gas: estimate.max_fee_per_gas,
                            max_priority_fee_per_gas: estimate.max_priority_fee_per_gas,
                        }
                    } else {
                        let gas_price = rpc.l1.get_gas_price().await?;
                        Fees::Legacy { gas_price }
                    }
                }
                fees => fees,
            };
            let mut transaction = match fees {
                Fees::Legacy { gas_price } => TypedTransaction::Legacy(TxLegacy {
                    chain_id: Some(chain_id),
                    nonce,
                    gas_price,
                    to: target.into(),
                    input,
                    ..Default::default()
                }),
                Fees::Eip1559 { max_fee_per_gas, max_priority_fee_per_gas } => {
                    TypedTransaction::Eip1559(TxEip1559 {
                        chain_id,
                        nonce,
                        max_fee_per_gas,
                        max_priority_fee_per_gas,
                        to: target.into(),
                        input,
                        ..Default::default()
                    })
                }
                Fees::Automatic => unreachable!("automatic fees were resolved"),
            };
            if let Some(cap) = config.max_gas_price {
                for fee in
                    [Some(transaction.max_fee_per_gas()), transaction.max_priority_fee_per_gas()]
                        .into_iter()
                        .flatten()
                {
                    if fee > cap {
                        return Err(WithdrawalTransactionError::FeeCap { fee, cap });
                    }
                }
            }
            let gas_limit = if let Some(gas_limit) = config.gas_limit {
                gas_limit
            } else {
                let mut request =
                    TransactionRequest::from_transaction_with_sender(transaction.clone(), sender);
                request.gas = None;
                request.nonce = None;
                let estimate = rpc.l1.estimate_gas(request).await?;
                Self::scale_gas_limit(estimate, config.gas_multiplier)?
            };
            match &mut transaction {
                TypedTransaction::Legacy(tx) => tx.gas_limit = gas_limit,
                TypedTransaction::Eip1559(tx) => tx.gas_limit = gas_limit,
                _ => unreachable!("only legacy and EIP-1559 calls are prepared"),
            }
            Ok(Self { sender, transaction })
        })
        .await
        .map_err(WithdrawalTransactionError::Timeout)?
    }

    /// Multiplies a gas estimate by a finite multiplier of at least one, rounding up.
    pub fn scale_gas_limit(
        estimate: u64,
        multiplier: f64,
    ) -> Result<u64, WithdrawalTransactionError> {
        if !multiplier.is_finite() || multiplier < 1.0 {
            return Err(WithdrawalTransactionError::InvalidGasMultiplier);
        }
        let gas = (estimate as f64 * multiplier).ceil();
        if !gas.is_finite() || gas >= u64::MAX as f64 {
            return Err(WithdrawalTransactionError::GasLimitOverflow);
        }
        Ok(gas as u64)
    }

    /// Maximum gas cost in wei, independent of the eventual effective gas price.
    pub fn maximum_gas_cost(&self) -> U256 {
        U256::from(self.transaction.gas_limit()) * U256::from(self.transaction.max_fee_per_gas())
    }

    /// Signs the prepared call without submitting it.
    pub async fn sign(
        mut self,
        signer: &WithdrawalSigner,
    ) -> Result<TxEnvelope, WithdrawalTransactionError> {
        if signer.address() != self.sender {
            return Err(WithdrawalTransactionError::SignerMismatch {
                sender: self.sender,
                signer: signer.address(),
            });
        }
        let signature = signer
            .sign_transaction(&mut self.transaction)
            .await
            .map_err(WithdrawalTransactionError::Signing)?;
        Ok(self.transaction.into_envelope(signature))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_consensus::{Header, Transaction};
    use alloy_network::TxSigner;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_provider::Provider;
    use alloy_rpc_types_eth::{Block, Header as RpcHeader};
    use axum::{Json, Router, extract::State, routing::post};
    use clap::Parser;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};
    use url::Url;

    use crate::{
        Fees, NetworkPreset, PreparedWithdrawalTransaction, WithdrawalRpc, WithdrawalSigner,
        WithdrawalTransactionError, WithdrawerArgs, WithdrawerConfig,
    };

    #[derive(Clone)]
    struct Script {
        requests: Arc<Mutex<Vec<Value>>>,
        base_fee: Option<u64>,
        gas_estimate: u64,
        response: Arc<Mutex<Option<(&'static str, Value)>>>,
    }

    struct Server {
        url: Url,
        requests: Arc<Mutex<Vec<Value>>>,
        task: JoinHandle<()>,
        response: Arc<Mutex<Option<(&'static str, Value)>>>,
    }

    impl Server {
        async fn new(base_fee: Option<u64>, gas_estimate: u64) -> Self {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let response = Arc::new(Mutex::new(None));
            let script = Script {
                requests: Arc::clone(&requests),
                base_fee,
                gas_estimate,
                response: Arc::clone(&response),
            };
            let router = Router::new()
                .fallback(post(
                    |State(script): State<Script>, Json(request): Json<Value>| async move {
                        let nonce = {
                            let mut requests = script.requests.lock().unwrap();
                            let nonce = 7 + requests
                                .iter()
                                .filter(|r| r["method"] == "eth_getTransactionCount")
                                .count();
                            requests.push(request.clone());
                            nonce
                        };
                        if let Some((method, mut response)) =
                            script.response.lock().unwrap().clone()
                            && request["method"] == method
                        {
                            response["jsonrpc"] = json!("2.0");
                            response["id"] = request["id"].clone();
                            return Json(response);
                        }
                        let result = match request["method"].as_str().unwrap() {
                            "eth_chainId" => json!("0x1"),
                            "eth_getTransactionCount" => json!(format!("0x{nonce:x}")),
                            "eth_getBlockByNumber" => {
                                json!(Block::<alloy_rpc_types_eth::Transaction>::empty(
                                    RpcHeader::new(Header {
                                        base_fee_per_gas: script.base_fee,
                                        ..Default::default()
                                    })
                                ))
                            }
                            "eth_feeHistory" => json!({
                                "oldestBlock":"0x1", "baseFeePerGas":["0x64", "0x64"],
                                "gasUsedRatio":[0.5], "reward":[["0x5"]]
                            }),
                            "eth_gasPrice" => json!("0x64"),
                            "eth_estimateGas" => json!(format!("0x{:x}", script.gas_estimate)),
                            method => panic!("unexpected RPC method {method}"),
                        };
                        Json(json!({"jsonrpc":"2.0", "id":request["id"], "result":result}))
                    },
                ))
                .with_state(script);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            Self { url, requests, task, response }
        }

        fn config(&self) -> WithdrawerConfig {
            let mut config = WithdrawerConfig::try_from(
                WithdrawerArgs::try_parse_from([
                    "withdrawer",
                    "--l1-rpc-url",
                    self.url.as_str(),
                    "--withdrawal",
                    &B256::ZERO.to_string(),
                    "--private-key",
                    &B256::with_last_byte(1).to_string(),
                ])
                .unwrap(),
            )
            .unwrap();
            config.network.l2_rpc_url = self.url.clone();
            config.max_retries = 0;
            config
        }

        fn respond(&self, method: &'static str, response: Value) {
            *self.response.lock().unwrap() = Some((method, response));
        }

        fn count(&self, method: &str) -> usize {
            self.requests.lock().unwrap().iter().filter(|r| r["method"] == method).count()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn automatic_final_fees_above_cap_are_rejected() {
        for base_fee in [Some(100), None] {
            let server = Server::new(base_fee, 21_001).await;
            let mut config = server.config();
            config.max_gas_price = Some(if base_fee.is_some() { 100 } else { 99 });
            let rpc = WithdrawalRpc::new(&config).unwrap();
            let result = PreparedWithdrawalTransaction::prepare(
                &config,
                &rpc,
                Address::ZERO,
                Address::repeat_byte(2),
                Bytes::new(),
            )
            .await;
            assert!(
                matches!(result, Err(WithdrawalTransactionError::FeeCap { .. })),
                "automatic final fees above the cap must fail before signing: {result:?}"
            );
            assert_eq!(server.count("eth_estimateGas"), 0);
        }
    }

    #[tokio::test]
    async fn l1_chain_must_match_the_network() {
        let server = Server::new(Some(100), 21_001).await;
        let mut config = server.config();
        config.network.l1_chain_id = Some(NetworkPreset::SEPOLIA_CHAIN_ID);
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let result = PreparedWithdrawalTransaction::prepare(
            &config,
            &rpc,
            Address::ZERO,
            Address::repeat_byte(2),
            Bytes::new(),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(WithdrawalTransactionError::L1ChainMismatch {
                    expected: NetworkPreset::SEPOLIA_CHAIN_ID,
                    actual: 1,
                })
            ),
            "{result:?}"
        );
        assert_eq!(server.count("eth_getTransactionCount"), 0);

        config.network.l1_chain_id = None;
        PreparedWithdrawalTransaction::prepare(
            &config,
            &rpc,
            Address::ZERO,
            Address::repeat_byte(2),
            Bytes::new(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn explicit_gas_limit_bypasses_estimation() {
        let server = Server::new(Some(100), 21_001).await;
        let mut config = server.config();
        config.fees = Fees::Legacy { gas_price: 5 };
        config.gas_limit = Some(30_000);
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let prepared = PreparedWithdrawalTransaction::prepare(
            &config,
            &rpc,
            Address::ZERO,
            Address::repeat_byte(2),
            Bytes::new(),
        )
        .await
        .expect("explicit gas limit must prepare without estimation");
        assert_eq!(server.count("eth_estimateGas"), 0, "explicit gas limit must bypass estimation");
        assert_eq!(prepared.transaction().gas_limit(), 30_000);
    }

    #[tokio::test]
    async fn gas_estimation_rounds_up_and_preserves_the_call() {
        for fees in [
            Fees::Legacy { gas_price: 5 },
            Fees::Eip1559 { max_fee_per_gas: 9, max_priority_fee_per_gas: 3 },
        ] {
            let server = Server::new(Some(100), 21_001).await;
            let mut config = server.config();
            config.fees = fees;
            config.gas_multiplier = 1.25;
            let rpc = WithdrawalRpc::new(&config).unwrap();
            let sender = Address::repeat_byte(1);
            let target = Address::repeat_byte(2);
            let input = Bytes::from_static(&[0xab, 0xcd]);
            let prepared = PreparedWithdrawalTransaction::prepare(
                &config,
                &rpc,
                sender,
                target,
                input.clone(),
            )
            .await
            .expect("gas estimation must prepare the call");
            assert_eq!(prepared.transaction().gas_limit(), 26_252, "gas limit must round up");
            let requests = server.requests.lock().unwrap();
            let estimate =
                &requests.iter().find(|r| r["method"] == "eth_estimateGas").unwrap()["params"][0];
            assert_eq!(estimate["from"], json!(sender));
            assert_eq!(estimate["to"], json!(target));
            assert_eq!(estimate["input"], json!(input));
            assert_eq!(estimate["chainId"], "0x1");
            assert!(estimate.get("nonce").is_none());
            assert_eq!(estimate["value"], "0x0");
            assert!(estimate.get("gas").is_none());
            match fees {
                Fees::Legacy { .. } => assert_eq!(estimate["gasPrice"], "0x5"),
                Fees::Eip1559 { .. } => {
                    assert_eq!(estimate["maxFeePerGas"], "0x9");
                    assert_eq!(estimate["maxPriorityFeePerGas"], "0x3");
                }
                Fees::Automatic => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn multiplied_gas_overflow_is_rejected() {
        let server = Server::new(None, u64::MAX).await;
        let mut config = server.config();
        config.fees = Fees::Legacy { gas_price: 5 };
        config.gas_multiplier = 2.0;
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let result = PreparedWithdrawalTransaction::prepare(
            &config,
            &rpc,
            Address::ZERO,
            Address::repeat_byte(2),
            Bytes::new(),
        )
        .await;
        assert!(
            matches!(result, Err(WithdrawalTransactionError::GasLimitOverflow)),
            "gas multiplication overflow must be rejected rather than saturated: {result:?}"
        );
    }

    #[tokio::test]
    async fn each_preparation_fetches_a_fresh_pending_nonce() {
        let server = Server::new(None, 21_001).await;
        let mut config = server.config();
        config.fees = Fees::Legacy { gas_price: 5 };
        config.gas_limit = Some(30_000);
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let mut nonces = Vec::new();
        for _ in 0..2 {
            nonces.push(
                PreparedWithdrawalTransaction::prepare(
                    &config,
                    &rpc,
                    Address::repeat_byte(1),
                    Address::repeat_byte(2),
                    Bytes::new(),
                )
                .await
                .expect("each call must prepare with its current nonce")
                .transaction()
                .nonce(),
            );
        }
        assert_eq!(nonces, [7, 8], "pending nonce must not be cached across preparations");
        let requests = server.requests.lock().unwrap();
        let nonce_requests = requests
            .iter()
            .filter(|r| r["method"] == "eth_getTransactionCount")
            .collect::<Vec<_>>();
        assert_eq!(nonce_requests.len(), 2);
        for request in nonce_requests {
            assert_eq!(request["params"], json!([Address::repeat_byte(1), "pending"]));
        }
    }

    #[tokio::test]
    async fn explicit_fees_are_capped_again_during_preparation() {
        let server = Server::new(Some(100), 21_001).await;
        let mut config = server.config();
        config.max_gas_price = Some(10);
        config.gas_limit = Some(30_000);
        let rpc = WithdrawalRpc::new(&config).unwrap();
        for fees in [
            Fees::Legacy { gas_price: 11 },
            Fees::Eip1559 { max_fee_per_gas: 11, max_priority_fee_per_gas: 3 },
            Fees::Eip1559 { max_fee_per_gas: 10, max_priority_fee_per_gas: 11 },
        ] {
            config.fees = fees;
            let result = PreparedWithdrawalTransaction::prepare(
                &config,
                &rpc,
                Address::ZERO,
                Address::repeat_byte(2),
                Bytes::new(),
            )
            .await;
            assert!(matches!(result, Err(WithdrawalTransactionError::FeeCap { fee: 11, cap: 10 })));
        }
        for fees in [
            Fees::Legacy { gas_price: 10 },
            Fees::Eip1559 { max_fee_per_gas: 10, max_priority_fee_per_gas: 10 },
        ] {
            config.fees = fees;
            PreparedWithdrawalTransaction::prepare(
                &config,
                &rpc,
                Address::ZERO,
                Address::repeat_byte(2),
                Bytes::new(),
            )
            .await
            .expect("fees equal to the cap must be accepted");
        }
    }

    #[tokio::test]
    async fn automatic_fees_select_the_supported_transaction_type() {
        for base_fee in [Some(100), Some(0), None] {
            let server = Server::new(base_fee, 21_001).await;
            let mut config = server.config();
            config.max_gas_price = Some(1_000);
            let rpc = WithdrawalRpc::new(&config).unwrap();
            let prepared = PreparedWithdrawalTransaction::prepare(
                &config,
                &rpc,
                Address::ZERO,
                Address::repeat_byte(2),
                Bytes::new(),
            )
            .await
            .unwrap();
            if base_fee.is_some() {
                let estimate = rpc.l1.estimate_eip1559_fees().await.unwrap();
                assert!(matches!(
                    prepared.transaction(),
                    alloy_consensus::TypedTransaction::Eip1559(_)
                ));
                assert_eq!(prepared.transaction().max_fee_per_gas(), estimate.max_fee_per_gas);
                assert_eq!(
                    prepared.transaction().max_priority_fee_per_gas(),
                    Some(estimate.max_priority_fee_per_gas)
                );
                assert_eq!(server.count("eth_gasPrice"), 0);
            } else {
                assert!(matches!(
                    prepared.transaction(),
                    alloy_consensus::TypedTransaction::Legacy(_)
                ));
                assert_eq!(prepared.transaction().gas_price(), Some(100));
                assert_eq!(server.count("eth_feeHistory"), 0);
            }
        }
    }

    #[test]
    fn gas_scaling_rounds_up_and_rejects_overflow() {
        assert_eq!(PreparedWithdrawalTransaction::scale_gas_limit(21_001, 1.5).unwrap(), 31_502);
        assert_eq!(PreparedWithdrawalTransaction::scale_gas_limit(21_001, 1.0).unwrap(), 21_001);
        for (estimate, multiplier) in [(u64::MAX, 1.0), (21_001, f64::MAX)] {
            assert!(matches!(
                PreparedWithdrawalTransaction::scale_gas_limit(estimate, multiplier),
                Err(WithdrawalTransactionError::GasLimitOverflow)
            ));
        }
    }

    #[tokio::test]
    async fn invalid_multipliers_fail_without_unchecked_arithmetic() {
        let server = Server::new(None, 21_001).await;
        let mut config = server.config();
        config.fees = Fees::Legacy { gas_price: 5 };
        let rpc = WithdrawalRpc::new(&config).unwrap();
        for multiplier in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.99] {
            config.gas_multiplier = multiplier;
            assert!(matches!(
                PreparedWithdrawalTransaction::prepare(
                    &config,
                    &rpc,
                    Address::ZERO,
                    Address::repeat_byte(2),
                    Bytes::new(),
                )
                .await,
                Err(WithdrawalTransactionError::InvalidGasMultiplier)
            ));
        }
    }

    #[tokio::test]
    async fn preparation_does_not_access_signing_material() {
        let server = Server::new(None, 21_001).await;
        let mut config = server.config();
        config.signer = crate::SignerSource::PrivateKey(crate::Secret(B256::ZERO));
        config.fees = Fees::Legacy { gas_price: u128::MAX };
        config.gas_limit = Some(u64::MAX);
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let prepared = PreparedWithdrawalTransaction::prepare(
            &config,
            &rpc,
            Address::ZERO,
            Address::repeat_byte(2),
            Bytes::new(),
        )
        .await
        .expect("preparation must not construct a signer, even with an invalid signing key");
        assert_eq!(prepared.sender(), Address::ZERO);
        assert_eq!(prepared.maximum_gas_cost(), U256::from(u64::MAX) * U256::from(u128::MAX));
    }

    #[tokio::test]
    async fn signing_rejects_a_different_sender() {
        let server = Server::new(None, 21_001).await;
        let mut config = server.config();
        config.fees = Fees::Legacy { gas_price: 5 };
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let signer = WithdrawalSigner::new(&config.signer).await.unwrap();
        let prepared = PreparedWithdrawalTransaction::prepare(
            &config,
            &rpc,
            Address::ZERO,
            Address::repeat_byte(2),
            Bytes::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            prepared.sign(&signer).await,
            Err(WithdrawalTransactionError::SignerMismatch { sender: Address::ZERO, .. })
        ));
    }

    #[tokio::test]
    async fn missing_latest_block_is_not_a_legacy_fallback() {
        let server = Server::new(None, 21_001).await;
        server.respond("eth_getBlockByNumber", json!({"result":null}));
        let config = server.config();
        let rpc = WithdrawalRpc::new(&config).unwrap();
        assert!(matches!(
            PreparedWithdrawalTransaction::prepare(
                &config,
                &rpc,
                Address::ZERO,
                Address::repeat_byte(2),
                Bytes::new(),
            )
            .await,
            Err(WithdrawalTransactionError::MissingLatestBlock)
        ));
        assert_eq!(server.count("eth_gasPrice"), 0);
    }

    #[tokio::test]
    async fn rpc_rejections_preserve_their_source() {
        let server = Server::new(Some(100), 21_001).await;
        server
            .respond("eth_estimateGas", json!({"error":{"code":3,"message":"execution reverted"}}));
        let config = server.config();
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let error = PreparedWithdrawalTransaction::prepare(
            &config,
            &rpc,
            Address::ZERO,
            Address::repeat_byte(2),
            Bytes::new(),
        )
        .await
        .unwrap_err();
        let source =
            error.source().unwrap().downcast_ref::<alloy_transport::TransportError>().unwrap();
        assert_eq!(source.as_error_resp().unwrap().code, 3);
        assert_eq!(source.as_error_resp().unwrap().message, "execution reverted");
    }

    #[tokio::test]
    async fn typed_rpc_decode_errors_do_not_expose_response_secrets() {
        for (method, base_fee) in [
            ("eth_chainId", Some(100)),
            ("eth_getTransactionCount", Some(100)),
            ("eth_getBlockByNumber", Some(100)),
            ("eth_feeHistory", Some(100)),
            ("eth_gasPrice", None),
            ("eth_estimateGas", Some(100)),
        ] {
            let server = Server::new(base_fee, 21_001).await;
            server.respond(
                method,
                json!({"result":"https://user:password@example.com/path-secret?key=query-secret"}),
            );
            let config = server.config();
            let rpc = WithdrawalRpc::new(&config).unwrap();
            let error = PreparedWithdrawalTransaction::prepare(
                &config,
                &rpc,
                Address::ZERO,
                Address::repeat_byte(2),
                Bytes::new(),
            )
            .await
            .unwrap_err();
            let mut rendered = format!("{error} {error:?}");
            let mut source = error.source();
            assert!(source.is_some());
            while let Some(cause) = source {
                rendered.push_str(&format!(" {cause} {cause:?}"));
                source = cause.source();
            }
            for secret in ["password", "path-secret", "query-secret"] {
                assert!(!rendered.contains(secret), "{method} exposed a secret: {rendered}");
            }
        }
    }

    #[tokio::test]
    async fn connection_errors_keep_safe_sources_without_endpoint_credentials() {
        let server = Server::new(None, 21_001).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut config = server.config();
        config.l1_rpc_url =
            format!("http://rpc-user:rpc-password@127.0.0.1:{port}/path-secret?key=query-secret")
                .parse()
                .unwrap();
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let error = PreparedWithdrawalTransaction::prepare(
            &config,
            &rpc,
            Address::ZERO,
            Address::repeat_byte(2),
            Bytes::new(),
        )
        .await
        .unwrap_err();
        let mut rendered = format!("{error} {error:?}");
        let mut source = error.source();
        let mut retained_request_error = false;
        while let Some(cause) = source {
            retained_request_error |= cause.downcast_ref::<reqwest::Error>().is_some();
            rendered.push_str(&format!(" {cause} {cause:?}"));
            source = cause.source();
        }
        assert!(retained_request_error, "connection errors must retain the safe reqwest source");
        for secret in ["rpc-user", "rpc-password", "path-secret", "query-secret"] {
            assert!(!rendered.contains(secret), "endpoint credential leaked: {rendered}");
        }
    }

    #[tokio::test]
    async fn preparation_deadline_bounds_all_provider_requests() {
        let server = Server::new(None, 21_001).await;
        let config = server.config();
        let mut rpc = WithdrawalRpc::new(&config).unwrap();
        rpc.timeout = Duration::ZERO;
        assert!(matches!(
            PreparedWithdrawalTransaction::prepare(
                &config,
                &rpc,
                Address::ZERO,
                Address::repeat_byte(2),
                Bytes::new(),
            )
            .await,
            Err(WithdrawalTransactionError::Timeout(_))
        ));
    }

    #[tokio::test]
    async fn signing_preserves_prepared_legacy_and_eip1559_calls() {
        for fees in [
            Fees::Legacy { gas_price: 5 },
            Fees::Eip1559 { max_fee_per_gas: 9, max_priority_fee_per_gas: 3 },
        ] {
            let server = Server::new(Some(100), 21_001).await;
            let mut config = server.config();
            config.fees = fees;
            config.gas_limit = Some(30_000);
            let signer = WithdrawalSigner::new(&config.signer).await.unwrap();
            let rpc = WithdrawalRpc::new(&config).unwrap();
            let target = Address::repeat_byte(2);
            let input = Bytes::from_static(&[0xab, 0xcd]);
            let prepared = PreparedWithdrawalTransaction::prepare(
                &config,
                &rpc,
                signer.address(),
                target,
                input.clone(),
            )
            .await
            .unwrap();
            let signed = prepared.sign(&signer).await.unwrap();
            assert_eq!(
                signed.signature().recover_address_from_prehash(&signed.signature_hash()).unwrap(),
                signer.address(),
            );
            assert_eq!(signed.chain_id(), Some(1));
            assert_eq!(signed.nonce(), 7);
            assert_eq!(signed.gas_limit(), 30_000);
            assert_eq!(signed.to(), Some(target));
            assert_eq!(signed.input(), &input);
            assert_eq!(signed.value(), U256::ZERO);
            match fees {
                Fees::Legacy { gas_price } => {
                    assert!(signed.is_legacy());
                    assert_eq!(signed.gas_price(), Some(gas_price));
                }
                Fees::Eip1559 { max_fee_per_gas, max_priority_fee_per_gas } => {
                    assert!(signed.is_eip1559());
                    assert_eq!(signed.max_fee_per_gas(), max_fee_per_gas);
                    assert_eq!(signed.max_priority_fee_per_gas(), Some(max_priority_fee_per_gas));
                }
                Fees::Automatic => unreachable!(),
            }
        }
    }
}
