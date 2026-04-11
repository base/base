use core::cmp::max;
use std::{collections::VecDeque, sync::Arc};

use alloy_consensus::TxEip1559;
use alloy_eips::{BlockNumberOrTag, eip1559::MIN_PROTOCOL_BASE_FEE, eip2718::Encodable2718};
use alloy_primitives::{Address, Bytes, TxHash, TxKind, U256, hex};
use alloy_provider::{PendingTransactionBuilder, Provider, RootProvider};
use base_common_consensus::{BaseTxEnvelope, BaseTypedTransaction};
use base_common_network::Base;
use base_execution_txpool::BasePooledTransaction;
use dashmap::DashMap;
use futures::StreamExt;
use reth_primitives::Recovered;
use reth_transaction_pool::{AllTransactionsEvents, FullTransactionEvent, TransactionEvent};
use tokio::sync::watch;
use tracing::debug;

use super::{PrivateKeySigner, funded_signer, sign_op_tx};

/// Builder for constructing and sending EIP-1559 transactions in tests.
#[derive(Clone, Debug)]
pub struct TransactionBuilder {
    provider: RootProvider<Base>,
    signer: Option<PrivateKeySigner>,
    nonce: Option<u64>,
    base_fee: Option<u128>,
    tx: TxEip1559,
}

impl TransactionBuilder {
    /// Creates a new builder with default EIP-1559 parameters for the test chain (chain ID 901).
    pub fn new(provider: RootProvider<Base>) -> Self {
        Self {
            provider,
            signer: None,
            nonce: None,
            base_fee: None,
            tx: TxEip1559 { chain_id: 901, gas_limit: 210000, ..Default::default() },
        }
    }

    /// Sets the transaction recipient address.
    pub const fn with_to(mut self, to: Address) -> Self {
        self.tx.to = TxKind::Call(to);
        self
    }

    /// Sets the transaction to a contract creation (no recipient).
    pub const fn with_create(mut self) -> Self {
        self.tx.to = TxKind::Create;
        self
    }

    /// Sets the transfer value in wei.
    pub fn with_value(mut self, value: u128) -> Self {
        self.tx.value = U256::from(value);
        self
    }

    /// Sets the private key used to sign the transaction.
    pub fn with_signer(mut self, signer: &PrivateKeySigner) -> Self {
        self.signer = Some(signer.clone());
        self
    }

    /// Overrides the chain ID (default: 901).
    pub const fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.tx.chain_id = chain_id;
        self
    }

    /// Sets an explicit nonce instead of fetching it from the provider.
    pub const fn with_nonce(mut self, nonce: u64) -> Self {
        self.tx.nonce = nonce;
        self
    }

    /// Sets the gas limit for the transaction.
    pub const fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.tx.gas_limit = gas_limit;
        self
    }

    /// Sets the EIP-1559 max fee per gas.
    pub const fn with_max_fee_per_gas(mut self, max_fee_per_gas: u128) -> Self {
        self.tx.max_fee_per_gas = max_fee_per_gas;
        self
    }

    /// Sets the EIP-1559 max priority fee per gas (tip).
    pub const fn with_max_priority_fee_per_gas(mut self, max_priority_fee_per_gas: u128) -> Self {
        self.tx.max_priority_fee_per_gas = max_priority_fee_per_gas;
        self
    }

    /// Sets the calldata input bytes.
    pub fn with_input(mut self, input: Bytes) -> Self {
        self.tx.input = input;
        self
    }

    /// Sets calldata that immediately reverts (`PUSH1 0 PUSH1 0 REVERT`).
    pub fn with_revert(mut self) -> Self {
        self.tx.input = hex!("60006000fd").into();
        self
    }

    /// Signs the transaction and returns it as a recovered envelope. Auto-fetches nonce and base
    /// fee from the provider when not explicitly set.
    pub async fn build(mut self) -> Recovered<BaseTxEnvelope> {
        let signer = self.signer.unwrap_or_else(funded_signer);

        let nonce = match self.nonce {
            Some(nonce) => nonce,
            None => self
                .provider
                .get_transaction_count(signer.address())
                .pending()
                .await
                .expect("Failed to get transaction count"),
        };

        let base_fee = match self.base_fee {
            Some(base_fee) => base_fee,
            None => {
                let previous_base_fee = self
                    .provider
                    .get_block_by_number(BlockNumberOrTag::Latest)
                    .await
                    .expect("failed to get latest block")
                    .expect("latest block should exist")
                    .header
                    .base_fee_per_gas
                    .expect("base fee should be present in latest block");

                max(previous_base_fee as u128, MIN_PROTOCOL_BASE_FEE as u128)
            }
        };

        self.tx.nonce = nonce;

        if self.tx.max_fee_per_gas == 0 {
            self.tx.max_fee_per_gas = base_fee + self.tx.max_priority_fee_per_gas;
        }

        sign_op_tx(&signer, BaseTypedTransaction::Eip1559(self.tx))
            .expect("Failed to sign transaction")
    }

    /// Builds, signs, and broadcasts the transaction, returning a pending transaction handle.
    pub async fn send(self) -> eyre::Result<PendingTransactionBuilder<Base>> {
        let provider = self.provider.clone();
        let transaction = self.build().await;
        let transaction_encoded = transaction.encoded_2718();

        Ok(provider.send_raw_transaction(transaction_encoded.as_slice()).await?)
    }
}

type ObservationsMap = DashMap<TxHash, VecDeque<TransactionEvent>>;

/// Monitors transaction pool events in the background, recording per-transaction lifecycle
/// history.
#[derive(Debug)]
pub struct TransactionPoolObserver {
    /// Stores a mapping of all observed transactions to their history of events.
    observations: Arc<ObservationsMap>,

    /// Fired when this type is dropped, giving a signal to the listener loop
    /// to stop listening for events.
    term: Option<watch::Sender<bool>>,
}

impl Drop for TransactionPoolObserver {
    fn drop(&mut self) {
        // Signal the listener loop to stop listening for events
        if let Some(term) = self.term.take() {
            let _ = term.send(true);
        }
    }
}

impl TransactionPoolObserver {
    /// Spawns a background listener that records all pool events from the given stream.
    pub fn new(stream: AllTransactionsEvents<BasePooledTransaction>) -> Self {
        let mut stream = stream;
        let observations = Arc::new(ObservationsMap::new());
        let observations_clone = Arc::clone(&observations);
        let (term, mut term_rx) = watch::channel(false);

        tokio::spawn(async move {
            let observations = observations_clone;

            loop {
                tokio::select! {
                    _ = term_rx.changed() => {
                        if *term_rx.borrow() {
                            debug!("Transaction pool observer terminated.");
                            return;
                        }
                    }
                    tx_event = stream.next() => {
                        match tx_event {
                            Some(FullTransactionEvent::Pending(hash)) => {
                                debug!(hash = %hash, "Transaction pending");
                                observations.entry(hash).or_default().push_back(TransactionEvent::Pending);
                            },
                            Some(FullTransactionEvent::Queued(hash, _)) => {
                                debug!(hash = %hash, "Transaction queued");
                                observations.entry(hash).or_default().push_back(TransactionEvent::Queued);
                            },
                            Some(FullTransactionEvent::Mined { tx_hash, block_hash }) => {
                                debug!(tx_hash = %tx_hash, block_hash = %block_hash, "Transaction mined");
                                observations.entry(tx_hash).or_default().push_back(TransactionEvent::Mined(block_hash));
                            },
                            Some(FullTransactionEvent::Replaced { transaction, replaced_by }) => {
                                debug!(transaction = ?transaction, replaced_by = %replaced_by, "Transaction replaced");
                                observations.entry(*transaction.hash()).or_default().push_back(TransactionEvent::Replaced(replaced_by));
                            },
                            Some(FullTransactionEvent::Discarded(hash)) => {
                                debug!(hash = %hash, "Transaction discarded");
                                observations.entry(hash).or_default().push_back(TransactionEvent::Discarded);
                            },
                            Some(FullTransactionEvent::Invalid(hash)) => {
                                debug!(hash = %hash, "Transaction invalid");
                                observations.entry(hash).or_default().push_back(TransactionEvent::Invalid);
                            },
                            Some(FullTransactionEvent::Propagated(_)) | None => {},
                        }
                    }
                }
            }
        });

        Self { observations, term: Some(term) }
    }

    /// Returns the most recent pool event for the given transaction hash.
    pub fn tx_status(&self, txhash: TxHash) -> Option<TransactionEvent> {
        self.observations.get(&txhash).and_then(|history| history.back().cloned())
    }

    /// Returns `true` if the transaction is currently in the pending sub-pool.
    pub fn is_pending(&self, txhash: TxHash) -> bool {
        matches!(self.tx_status(txhash), Some(TransactionEvent::Pending))
    }

    /// Returns `true` if the transaction is currently queued.
    pub fn is_queued(&self, txhash: TxHash) -> bool {
        matches!(self.tx_status(txhash), Some(TransactionEvent::Queued))
    }

    /// Returns `true` if the transaction has been discarded from the pool.
    pub fn is_dropped(&self, txhash: TxHash) -> bool {
        matches!(self.tx_status(txhash), Some(TransactionEvent::Discarded))
    }

    /// Counts how many observed transactions currently have the given status.
    pub fn count(&self, status: TransactionEvent) -> usize {
        self.observations.iter().filter(|tx| tx.value().back() == Some(&status)).count()
    }

    /// Returns the number of transactions in the pending state.
    pub fn pending_count(&self) -> usize {
        self.count(TransactionEvent::Pending)
    }

    /// Returns the number of transactions in the queued state.
    pub fn queued_count(&self) -> usize {
        self.count(TransactionEvent::Queued)
    }

    /// Returns the number of discarded transactions.
    pub fn dropped_count(&self) -> usize {
        self.count(TransactionEvent::Discarded)
    }

    /// Returns the history of pool events for a transaction.
    pub fn history(&self, txhash: TxHash) -> Option<Vec<TransactionEvent>> {
        self.observations.get(&txhash).map(|history| history.iter().cloned().collect())
    }

    /// Logs all observed transaction pool events at debug level.
    pub fn print_all(&self) {
        debug!(observations = ?self.observations, "TxPool");
    }

    /// Returns `true` if the transaction is either pending or queued in the pool.
    pub fn exists(&self, txhash: TxHash) -> bool {
        matches!(
            self.tx_status(txhash),
            Some(TransactionEvent::Pending) | Some(TransactionEvent::Queued)
        )
    }
}
