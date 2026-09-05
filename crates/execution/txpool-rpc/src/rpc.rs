//! RPC implementation for transaction submission, status queries, and pool management.

use alloy_consensus::{BlockHeader, Typed2718};
use alloy_primitives::{Address, Bytes, TxHash};
use base_common_chains::Upgrades;
use base_common_consensus::EIP8130_TX_TYPE_ID;
use base_execution_txpool::{
    BasePooledTransaction, DEFAULT_MAX_VALIDITY_PREDICATES, ValidityPredicate,
};
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use jsonrpsee::{
    core::{RpcResult, async_trait, client::ClientT},
    http_client::{HttpClient, HttpClientBuilder},
    proc_macros::rpc,
    rpc_params,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_chainspec::ChainSpecProvider;
use reth_rpc_eth_types::error::RpcPoolError;
use reth_storage_api::BlockReaderIdExt;
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Rejection message returned when an EIP-8130 (account abstraction) validity transaction is
/// submitted before the Zenith hard fork is active at the latest block.
///
/// EIP-8130 validity transactions are fork-gated on Zenith; other transaction types (e.g. EIP-1559)
/// carry validity predicates under the experimental flag alone.
pub const VALIDITY_TX_PRE_ZENITH_RPC_ERROR: &str = "EIP-8130 validity transactions are gated behind \
     the Zenith hard fork; they are not accepted before Zenith is active";

/// The status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub enum Status {
    /// Transaction is not known to the node.
    Unknown,
    /// Transaction is known to the node (in mempool or confirmed).
    Known,
}

/// Response containing the status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct TransactionStatusResponse {
    /// The status of the queried transaction.
    pub status: Status,
}

/// Options for `base_sendRawTransactionValidity` accompanying the raw transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct SendRawTransactionValidityOptions {
    /// Experimental predicates transported to builders alongside the transaction.
    pub validity: Vec<ValidityPredicate>,
}

/// RPC API for transaction status
#[rpc(server, namespace = "base")]
pub trait TransactionStatusApi {
    /// Gets the status of a transaction
    #[method(name = "transactionStatus")]
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<TransactionStatusResponse>;
}

/// Experimental RPC API for submitting a raw transaction with validity criteria.
#[rpc(server, namespace = "base")]
pub trait SendRawTransactionValidityApi {
    /// Submits a raw transaction and transports its currently unenforced validity criteria.
    #[method(name = "sendRawTransactionValidity")]
    async fn send_raw_transaction_validity(
        &self,
        tx: Bytes,
        options: SendRawTransactionValidityOptions,
    ) -> RpcResult<TxHash>;
}

/// Admin RPC API for transaction pool management operations.
///
/// Complements the upstream `admin_clearTxpool` method provided by reth,
/// which removes all transactions from the pool.
#[rpc(server, namespace = "admin")]
pub trait AdminTxPoolApi {
    /// Drops all transactions from a specific sender address.
    #[method(name = "dropSenderTransactions")]
    async fn drop_sender_transactions(&self, sender: Address) -> RpcResult<Vec<TxHash>>;

    /// Drops a single transaction by its hash.
    #[method(name = "dropTransaction")]
    async fn drop_transaction(&self, tx_hash: TxHash) -> RpcResult<bool>;
}

/// Implementation of the Base transaction pool RPC APIs.
#[derive(Debug, Clone)]
pub struct TransactionStatusApiImpl<Pool: TransactionPool> {
    sequencer_client: Option<HttpClient>,
    pool: Pool,
}

/// Local mempool-ingress implementation for validity-bearing transactions.
#[derive(Debug, Clone)]
pub struct SendRawTransactionValidityApiImpl<Pool, Provider> {
    pool: Pool,
    provider: Provider,
    max_validity_predicates: usize,
}

impl<Pool, Provider> SendRawTransactionValidityApiImpl<Pool, Provider> {
    /// Creates a validity transaction ingress backed by the given pool and default predicate limit.
    ///
    /// The provider fork-gates the RPC method on the Zenith hard fork.
    pub const fn new(pool: Pool, provider: Provider) -> Self {
        Self { pool, provider, max_validity_predicates: DEFAULT_MAX_VALIDITY_PREDICATES }
    }

    /// Creates a validity transaction ingress with a predicate limit.
    ///
    /// The provider fork-gates the RPC method on the Zenith hard fork.
    pub const fn with_max_validity_predicates(
        pool: Pool,
        provider: Provider,
        max_validity_predicates: usize,
    ) -> Self {
        Self { pool, provider, max_validity_predicates }
    }
}

impl<Pool, Provider> SendRawTransactionValidityApiImpl<Pool, Provider>
where
    Provider: BlockReaderIdExt + ChainSpecProvider<ChainSpec: Upgrades>,
{
    /// Returns whether the Zenith hard fork is active at the latest block's timestamp.
    ///
    /// Returns `false` when no latest header is available (e.g. before genesis is committed), which
    /// keeps the fork-gated RPC method closed until a canonical head exists.
    fn is_zenith_active_at_latest(&self) -> RpcResult<bool> {
        let Some(header) = self.provider.latest_header().map_err(|error| {
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                format!("failed to resolve latest header: {error}"),
                None::<()>,
            )
        })?
        else {
            return Ok(false);
        };
        Ok(self.provider.chain_spec().is_zenith_active_at_timestamp(header.timestamp()))
    }
}

impl<Pool: TransactionPool + 'static> TransactionStatusApiImpl<Pool> {
    /// Creates a new transaction status API instance.
    ///
    /// If `sequencer_url` is provided, transaction status queries are forwarded to the
    /// sequencer. Otherwise, the local transaction pool is used.
    pub fn new(
        sequencer_url: Option<String>,
        pool: Pool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let sequencer_client = if let Some(ref url) = sequencer_url {
            debug!("fetching transaction status from sequencer");
            Some(HttpClientBuilder::default().build(url)?)
        } else {
            debug!("fetching transaction status from local transaction pool");
            None
        };

        Ok(Self { sequencer_client, pool })
    }
}

#[async_trait]
impl<Pool: TransactionPool + 'static> TransactionStatusApiServer
    for TransactionStatusApiImpl<Pool>
{
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<TransactionStatusResponse> {
        let Some(ref sequencer_client) = self.sequencer_client else {
            return Ok(match self.pool.get(&tx_hash) {
                Some(_) => TransactionStatusResponse { status: Status::Known },
                None => TransactionStatusResponse { status: Status::Unknown },
            });
        };

        match sequencer_client
            .request::<TransactionStatusResponse, _>("base_transactionStatus", rpc_params![tx_hash])
            .await
        {
            Ok(result) => Ok(result),
            Err(e) => {
                warn!(tx_hash = %tx_hash, error = %e, "failed to fetch transaction status");
                Err(ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("failed to fetch transaction status: {e}"),
                    None::<()>,
                ))
            }
        }
    }
}

#[async_trait]
impl<Pool, Provider> SendRawTransactionValidityApiServer
    for SendRawTransactionValidityApiImpl<Pool, Provider>
where
    Pool: TransactionPool<Transaction = BasePooledTransaction> + 'static,
    Provider: BlockReaderIdExt + ChainSpecProvider<ChainSpec: Upgrades> + 'static,
{
    async fn send_raw_transaction_validity(
        &self,
        tx: Bytes,
        options: SendRawTransactionValidityOptions,
    ) -> RpcResult<TxHash> {
        ValidityPredicate::validate_batch(&options.validity, self.max_validity_predicates)
            .map_err(|error| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    error.to_string(),
                    None::<()>,
                )
            })?;

        let transaction =
            BasePooledTransaction::recover_raw_transaction(tx.as_ref()).map_err(|error| {
                ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    format!("failed to decode transaction: {error}"),
                    None::<()>,
                )
            })?;

        // EIP-8130 (account abstraction) validity transactions are fork-gated on Zenith. Other
        // transaction types (e.g. EIP-1559) carry validity predicates under the experimental flag
        // alone and are accepted before Zenith activates.
        if transaction.ty() == EIP8130_TX_TYPE_ID && !self.is_zenith_active_at_latest()? {
            return Err(ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                VALIDITY_TX_PRE_ZENITH_RPC_ERROR,
                None::<()>,
            ));
        }

        let tx_hash = *transaction.hash();
        let _ = transaction_event!(
            producer: TransactionEventProducer::BaseRethNode,
            event_type: TransactionEventType::TxpoolSendRawTransactionValidity,
            tx_hash: tx_hash,
            data: {
                "rpc_method" => "base_sendRawTransactionValidity",
                "validity_predicates" => &options.validity,
            },
        );

        // Retain predicates for canonical forwarding to builders.
        self.pool
            .add_transaction(
                TransactionOrigin::Private,
                transaction.with_validity_predicates(options.validity),
            )
            .await
            .map_err(|error| ErrorObjectOwned::from(RpcPoolError::from(error)))?;

        Ok(tx_hash)
    }
}

/// Implementation of the admin transaction pool management RPC API.
#[derive(Debug)]
pub struct AdminTxPoolApiImpl<Pool: TransactionPool> {
    pool: Pool,
}

impl<Pool: TransactionPool + 'static> AdminTxPoolApiImpl<Pool> {
    /// Creates a new admin transaction pool management API instance.
    pub const fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl<Pool: TransactionPool + 'static> AdminTxPoolApiServer for AdminTxPoolApiImpl<Pool> {
    async fn drop_sender_transactions(&self, sender: Address) -> RpcResult<Vec<TxHash>> {
        let removed = self.pool.remove_transactions_by_sender(sender);
        let hashes: Vec<TxHash> = removed.iter().map(|tx| *tx.hash()).collect();
        info!(sender = %sender, count = hashes.len(), "dropped transactions by sender");
        Ok(hashes)
    }

    async fn drop_transaction(&self, tx_hash: TxHash) -> RpcResult<bool> {
        let removed = self.pool.remove_transactions(vec![tx_hash]);
        let was_removed = !removed.is_empty();
        info!(tx_hash = %tx_hash, removed = was_removed, "dropped transaction");
        Ok(was_removed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, TxHash, TxKind, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, BasePrimitives, Eip8130Signed,
        TxEip8130,
    };
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_observability_events::{
        TransactionEventBuilder, TransactionEventCapture, TransactionEventProducer,
        TransactionEventType,
    };
    use httpmock::prelude::*;
    use reth_provider::test_utils::MockEthProvider;
    use reth_transaction_pool::{
        PoolTransaction, TransactionOrigin,
        noop::NoopTransactionPool,
        test_utils::{MockTransaction, testing_pool},
    };
    use serde_json::{self, json};

    use super::*;

    /// Provider whose latest header sits after Zenith activation, so the fork gate is open.
    fn zenith_provider() -> MockEthProvider<BasePrimitives, Arc<BaseChainSpec>> {
        MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::new(
                BaseChainSpecBuilder::base_mainnet().zenith_activated().build(),
            ))
            .with_genesis_block()
    }

    /// Provider whose latest header predates Zenith activation, so the fork gate is closed.
    fn pre_zenith_provider() -> MockEthProvider<BasePrimitives, Arc<BaseChainSpec>> {
        MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::new(
                BaseChainSpecBuilder::base_mainnet().cobalt_activated().build(),
            ))
            .with_genesis_block()
    }

    fn validity_pool() -> NoopTransactionPool<BasePooledTransaction> {
        NoopTransactionPool::<BasePooledTransaction>::new()
    }

    fn validity_request(tx: Bytes) -> (Bytes, SendRawTransactionValidityOptions) {
        (
            tx,
            SendRawTransactionValidityOptions {
                validity: vec![ValidityPredicate::Storage {
                    address: Address::repeat_byte(0xab),
                    slot: U256::from(1),
                    mask: U256::MAX,
                    op: base_execution_txpool::ValidityOperator::Equal,
                    value: U256::from(0x789),
                }],
            },
        )
    }

    fn all_predicate_variants() -> Vec<ValidityPredicate> {
        vec![
            ValidityPredicate::Balance {
                address: Address::repeat_byte(0x11),
                op: base_execution_txpool::ValidityOperator::GreaterThanOrEqual,
                value: U256::from(1),
            },
            ValidityPredicate::Storage {
                address: Address::repeat_byte(0xab),
                slot: U256::from(1),
                mask: U256::MAX,
                op: base_execution_txpool::ValidityOperator::Equal,
                value: U256::from(0x789),
            },
            ValidityPredicate::BlockNumber {
                op: base_execution_txpool::ValidityOperator::GreaterThanOrEqual,
                value: U256::from(100),
            },
            ValidityPredicate::FlashblockIndex {
                op: base_execution_txpool::ValidityOperator::LessThan,
                value: U256::from(5),
            },
        ]
    }

    fn signed_eip1559(signer: &PrivateKeySigner, nonce: u64, priority_fee: u128) -> Bytes {
        let tx = TxEip1559 {
            chain_id: 8453,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: priority_fee,
            to: TxKind::Call(Address::repeat_byte(0x11)),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Default::default(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("test signer");
        tx.into_signed(signature).encoded_2718().into()
    }

    /// Builds a raw, EOA-recoverable EIP-8130 transaction with a k1 sender authenticator.
    fn signed_eip8130(signer: &PrivateKeySigner) -> Bytes {
        let tx = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 1,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).expect("test signer");
        // The EOA sender authenticator is a canonical 65-byte `r || s || v` blob with `v` in
        // Electrum notation (`27`/`28`), which is what recovery requires.
        let mut sender_auth = [0u8; 65];
        sender_auth[..32].copy_from_slice(&signature.r().to_be_bytes::<32>());
        sender_auth[32..64].copy_from_slice(&signature.s().to_be_bytes::<32>());
        sender_auth[64] = 27 + u8::from(signature.v());
        let signed = Eip8130Signed::new(tx, Bytes::from(sender_auth.to_vec()), Bytes::new());
        ConsensusPooledTransaction::Eip8130(signed).encoded_2718().into()
    }

    #[test]
    fn send_raw_transaction_validity_request_uses_positional_params() {
        let (tx, options) = validity_request(Bytes::from_static(&[0x02]));

        // The raw transaction is the leading bare param, serialized as a hex string.
        let tx_value = serde_json::to_value(tx).expect("tx should serialize");
        assert_eq!(tx_value, "0x02");

        // The options object carries only `validity` alongside the transaction.
        let options_value = serde_json::to_value(options).expect("options should serialize");
        assert!(options_value.get("tx").is_none());
        assert_eq!(options_value["validity"][0]["type"], "storage");
        assert_eq!(options_value["validity"][0]["params"]["slot"], "0x1");
    }

    #[test]
    fn send_raw_transaction_validity_event_data_serializes_all_predicate_variants() {
        assert_eq!(
            serde_json::to_value(all_predicate_variants()).unwrap(),
            json!([
                {
                    "type": "balance",
                    "params": {
                        "address": "0x1111111111111111111111111111111111111111",
                        "op": ">=",
                        "value": "0x1",
                    },
                },
                {
                    "type": "storage",
                    "params": {
                        "address": "0xabababababababababababababababababababab",
                        "slot": "0x1",
                        "mask": "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                        "op": "=",
                        "value": "0x789",
                    },
                },
                {
                    "type": "block_number",
                    "params": {
                        "op": ">=",
                        "value": "0x64",
                    },
                },
                {
                    "type": "flashblock_index",
                    "params": {
                        "op": "<",
                        "value": "0x5",
                    },
                },
            ])
        );
    }

    #[test]
    fn send_raw_transaction_validity_event_envelope_joins_on_tx_hash() {
        let tx_hash = TxHash::repeat_byte(0x11);
        let event = TransactionEventBuilder::new(
            TransactionEventProducer::BaseRethNode,
            TransactionEventType::TxpoolSendRawTransactionValidity,
        )
        .tx_hash(tx_hash)
        .data_field("rpc_method", json!("base_sendRawTransactionValidity"))
        .data_field("validity_predicates", json!(all_predicate_variants()))
        .build_with_network("base-devnet");

        event.validate().expect("admission event should be valid");
        assert_eq!(event.event_type.to_string(), "TXPOOL_SEND_RAW_TRANSACTION_VALIDITY");
        assert_eq!(event.tx_hash, Some(tx_hash));
        assert!(event.data.contains_key("validity_predicates"));
    }

    #[tokio::test]
    async fn send_raw_transaction_validity_emits_predicate_list_on_admission() {
        let capture = TransactionEventCapture::install();
        let signer = PrivateKeySigner::random();
        let raw = signed_eip1559(&signer, 0, 1);
        let rpc = SendRawTransactionValidityApiImpl::new(validity_pool(), zenith_provider());
        let options = SendRawTransactionValidityOptions { validity: all_predicate_variants() };

        let tx_hash = rpc.send_raw_transaction_validity(raw, options).await.unwrap_or_else(|_| {
            capture.events().first().and_then(|event| event.tx_hash).expect(
                "admission event should fire before pool insertion even if the noop pool rejects",
            )
        });

        let events: Vec<_> = capture
            .events()
            .into_iter()
            .filter(|event| {
                event.event_type == TransactionEventType::TxpoolSendRawTransactionValidity
                    && event.tx_hash == Some(tx_hash)
            })
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["rpc_method"], "base_sendRawTransactionValidity");
        assert_eq!(
            events[0].data["validity_predicates"],
            serde_json::to_value(all_predicate_variants()).unwrap()
        );
    }

    #[test]
    fn send_raw_transaction_validity_method_is_registered() {
        let rpc = SendRawTransactionValidityApiImpl::new(validity_pool(), zenith_provider());
        let module = SendRawTransactionValidityApiServer::into_rpc(rpc);

        assert!(module.method_names().any(|name| name == "base_sendRawTransactionValidity"));
    }

    #[tokio::test]
    async fn send_raw_transaction_validity_rejects_eip8130_before_zenith() {
        let signer = PrivateKeySigner::random();
        let raw = signed_eip8130(&signer);
        let rpc = SendRawTransactionValidityApiImpl::new(validity_pool(), pre_zenith_provider());
        let options = SendRawTransactionValidityOptions { validity: all_predicate_variants() };

        let error = rpc
            .send_raw_transaction_validity(raw, options)
            .await
            .expect_err("EIP-8130 validity transactions should be rejected before Zenith");

        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert_eq!(error.message(), VALIDITY_TX_PRE_ZENITH_RPC_ERROR);
    }

    #[tokio::test]
    async fn send_raw_transaction_validity_accepts_eip1559_before_zenith() {
        // EIP-1559 validity transactions are gated by the experimental flag alone, not by Zenith,
        // so they clear the fork gate before Zenith activates. The admission event fires only once
        // the gate is cleared; the noop pool then rejects insertion.
        let capture = TransactionEventCapture::install();
        let signer = PrivateKeySigner::random();
        let raw = signed_eip1559(&signer, 0, 1);
        let rpc = SendRawTransactionValidityApiImpl::new(validity_pool(), pre_zenith_provider());
        let options = SendRawTransactionValidityOptions { validity: all_predicate_variants() };

        let error = rpc
            .send_raw_transaction_validity(raw, options)
            .await
            .expect_err("the noop pool rejects insertion after the fork gate is cleared");

        assert_ne!(error.message(), VALIDITY_TX_PRE_ZENITH_RPC_ERROR);
        assert!(
            capture
                .events()
                .iter()
                .any(|event| event.event_type
                    == TransactionEventType::TxpoolSendRawTransactionValidity),
            "admission event should fire once the Zenith gate is cleared for EIP-1559"
        );
    }

    #[tokio::test]
    async fn send_raw_transaction_validity_rejects_malformed_transaction() {
        let rpc = SendRawTransactionValidityApiImpl::new(validity_pool(), zenith_provider());

        let (raw, options) = validity_request(Bytes::from_static(&[0xff]));
        let error = rpc
            .send_raw_transaction_validity(raw, options)
            .await
            .expect_err("malformed transaction should be rejected");

        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert!(error.message().contains("failed to decode transaction"));
    }

    #[tokio::test]
    async fn send_raw_transaction_validity_enforces_configured_predicate_limit() {
        let rpc = SendRawTransactionValidityApiImpl::with_max_validity_predicates(
            validity_pool(),
            zenith_provider(),
            2,
        );
        let (raw, mut options) = validity_request(Bytes::from_static(&[0x02]));
        options.validity = vec![options.validity[0].clone(); 3];

        let error = rpc
            .send_raw_transaction_validity(raw, options)
            .await
            .expect_err("oversized validity should be rejected before transaction decoding");

        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert!(error.message().contains("too many validity predicates"));
        assert!(error.message().contains("maximum 2"));
    }

    #[tokio::test]
    async fn send_raw_transaction_validity_rejects_empty_predicates() {
        let rpc = SendRawTransactionValidityApiImpl::new(validity_pool(), zenith_provider());
        let (raw, mut options) = validity_request(Bytes::from_static(&[0x02]));
        options.validity.clear();

        let error = rpc
            .send_raw_transaction_validity(raw, options)
            .await
            .expect_err("empty validity should be rejected before transaction decoding");

        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert!(error.message().contains("must not be empty"));
    }

    #[tokio::test]
    async fn send_raw_transaction_validity_rejects_storage_value_outside_mask() {
        let rpc = SendRawTransactionValidityApiImpl::new(validity_pool(), zenith_provider());
        let (raw, mut options) = validity_request(Bytes::from_static(&[0x02]));
        options.validity = vec![ValidityPredicate::Storage {
            address: Address::repeat_byte(0xab),
            slot: U256::from(1),
            mask: U256::from(0xff),
            op: base_execution_txpool::ValidityOperator::Equal,
            value: U256::from(0x1ff),
        }];

        let error = rpc
            .send_raw_transaction_validity(raw, options)
            .await
            .expect_err("storage value outside its mask should be rejected");

        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert!(error.message().contains("outside its mask"));
    }

    #[tokio::test]
    async fn send_raw_transaction_validity_rejects_unsatisfiable_flashblock_index() {
        let rpc = SendRawTransactionValidityApiImpl::new(validity_pool(), zenith_provider());
        let (raw, mut options) = validity_request(Bytes::from_static(&[0x02]));
        // A flashblock-index predicate that only holds at index 0, which pooled
        // transactions never reach, would park forever if admitted.
        options.validity = vec![ValidityPredicate::FlashblockIndex {
            op: base_execution_txpool::ValidityOperator::Equal,
            value: U256::ZERO,
        }];

        let error = rpc
            .send_raw_transaction_validity(raw, options)
            .await
            .expect_err("an unsatisfiable flashblock-index predicate should be rejected");

        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert!(error.message().contains("can never be satisfied"));
    }

    #[tokio::test]
    async fn test_transaction_status() -> eyre::Result<()> {
        let pool = testing_pool();
        let rpc =
            TransactionStatusApiImpl::new(None, pool.clone()).expect("should be able to init rpc");

        let result = rpc
            .transaction_status(TxHash::random())
            .await
            .expect("should be able to fetch status")
            .status;
        assert_eq!(Status::Unknown, result);

        let tx = MockTransaction::eip1559();
        let hash = *tx.hash();

        let before = rpc
            .transaction_status(hash)
            .await
            .expect("should be able to fetch transaction status")
            .status;
        pool.add_transaction(TransactionOrigin::Local, tx)
            .await
            .expect("should be able to add local transaction");
        let after = rpc
            .transaction_status(hash)
            .await
            .expect("should be able to fetch transaction status")
            .status;

        assert_eq!(Status::Unknown, before);
        assert_eq!(Status::Known, after);

        Ok(())
    }

    #[tokio::test]
    async fn test_remote_status_failures() -> eyre::Result<()> {
        let tx = TxHash::random();

        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST)
                .path("/")
                .json_body(json!({"jsonrpc": "2.0", "id": 0, "method": "base_transactionStatus", "params": [tx]}));
            then.status(500);
        });

        let rpc = TransactionStatusApiImpl::new(Some(sequencer.base_url()), testing_pool())
            .expect("should be able to init rpc");

        let status = rpc.transaction_status(tx).await;
        assert!(status.is_err());

        mock.assert();

        Ok(())
    }

    #[tokio::test]
    async fn test_remote_success() -> eyre::Result<()> {
        let known_tx = TxHash::random();
        let unknown_tx = TxHash::random();

        let sequencer = MockServer::start();
        let rpc = TransactionStatusApiImpl::new(Some(sequencer.base_url()), testing_pool())
            .expect("should be able to init rpc");

        let response = |id: u8, status: Status| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "status": status
                }
            })
        };

        let known_mock = sequencer.mock(|when, then| {
            when.method(POST)
                .path("/")
                .json_body(json!({"jsonrpc": "2.0", "id": 0, "method": "base_transactionStatus", "params": [known_tx]}));
            then.status(200)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&response(0, Status::Known)).unwrap());
        });

        let status = rpc
            .transaction_status(known_tx)
            .await
            .expect("should be able to fetch transaction status");
        assert_eq!(Status::Known, status.status);
        known_mock.assert();

        let unknown_mock = sequencer.mock(|when, then| {
            when.method(POST)
                .path("/")
                .json_body(json!({"jsonrpc": "2.0", "id": 1, "method": "base_transactionStatus", "params": [unknown_tx]}));
            then.status(200)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&response(1, Status::Unknown)).unwrap());
        });

        let status = rpc
            .transaction_status(unknown_tx)
            .await
            .expect("should be able to fetch transaction status");
        assert_eq!(Status::Unknown, status.status);
        unknown_mock.assert();

        Ok(())
    }

    #[tokio::test]
    async fn test_drop_sender_no_transactions() {
        let pool = testing_pool();
        let rpc = AdminTxPoolApiImpl::new(pool);

        let sender = Address::random();
        let removed = rpc.drop_sender_transactions(sender).await.expect("should succeed");
        assert!(removed.is_empty());
    }

    #[tokio::test]
    async fn test_drop_sender_with_transactions() {
        let pool = testing_pool();

        let sender1 = Address::random();
        let sender2 = Address::random();

        // Add transactions from two different senders (different nonces to avoid replacement)
        let tx1 = MockTransaction::eip1559().with_sender(sender1).with_nonce(0);
        let tx2 = MockTransaction::eip1559().with_sender(sender1).with_nonce(1);
        let tx3 = MockTransaction::eip1559().with_sender(sender2).with_nonce(0);

        let hash1 = *tx1.hash();
        let hash2 = *tx2.hash();
        let hash3 = *tx3.hash();

        pool.add_transaction(TransactionOrigin::Local, tx1).await.expect("should add tx1");
        pool.add_transaction(TransactionOrigin::Local, tx2).await.expect("should add tx2");
        pool.add_transaction(TransactionOrigin::Local, tx3).await.expect("should add tx3");

        let rpc = AdminTxPoolApiImpl::new(pool.clone());

        // Remove sender1's transactions
        let removed = rpc.drop_sender_transactions(sender1).await.expect("should succeed");
        assert_eq!(2, removed.len());
        assert!(removed.contains(&hash1));
        assert!(removed.contains(&hash2));

        // sender2's transaction should still be in pool
        let remaining = pool.all_transaction_hashes();
        assert_eq!(1, remaining.len());
        assert!(remaining.contains(&hash3));
    }

    #[tokio::test]
    async fn test_drop_transaction_not_found() {
        let pool = testing_pool();
        let rpc = AdminTxPoolApiImpl::new(pool);

        let result = rpc.drop_transaction(TxHash::random()).await.expect("should succeed");
        assert!(!result);
    }

    #[tokio::test]
    async fn test_drop_transaction_found() {
        let pool = testing_pool();

        let tx = MockTransaction::eip1559();
        let hash = *tx.hash();

        pool.add_transaction(TransactionOrigin::Local, tx).await.expect("should add tx");

        let rpc = AdminTxPoolApiImpl::new(pool.clone());

        let result = rpc.drop_transaction(hash).await.expect("should succeed");
        assert!(result);

        // Verify tx is gone
        assert!(pool.get(&hash).is_none());
    }

    #[tokio::test]
    async fn test_drop_transaction_idempotent() {
        let pool = testing_pool();

        let tx = MockTransaction::eip1559();
        let hash = *tx.hash();

        pool.add_transaction(TransactionOrigin::Local, tx).await.expect("should add tx");

        let rpc = AdminTxPoolApiImpl::new(pool);

        let first = rpc.drop_transaction(hash).await.expect("should succeed");
        assert!(first);

        // Second removal should return false
        let second = rpc.drop_transaction(hash).await.expect("should succeed");
        assert!(!second);
    }
}
