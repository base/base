use core::cmp::max;
use std::{collections::VecDeque, sync::Arc};

use alloy_consensus::TxEip1559;
use alloy_eips::{BlockNumberOrTag, eip1559::MIN_PROTOCOL_BASE_FEE, eip2718::Encodable2718};
use alloy_primitives::{Address, Bytes, TxHash, TxKind, U256, hex};
use alloy_provider::{PendingTransactionBuilder, Provider, RootProvider};
use base_alloy_consensus::{OpTxEnvelope, OpTypedTransaction};
use base_alloy_network::Base;
use dashmap::DashMap;
use futures::StreamExt;
use reth_optimism_txpool::OpPooledTransaction;
use reth_primitives::Recovered;
use reth_transaction_pool::{AllTransactionsEvents, FullTransactionEvent, TransactionEvent};
use tokio::sync::watch;
use tracing::debug;

use super::{PrivateKeySigner, funded_signer, sign_op_tx};

#[derive(Clone, Debug)]
pub struct TransactionBuilder {
    provider: RootProvider<Base>,
    signer: Option<PrivateKeySigner>,
    nonce: Option<u64>,
    base_fee: Option<u128>,
    tx: TxEip1559,
}

impl TransactionBuilder {
    pub fn new(provider: RootProvider<Base>) -> Self {
        Self {
            provider,
            signer: None,
            nonce: None,
            base_fee: None,
            tx: TxEip1559 { chain_id: 901, gas_limit: 210000, ..Default::default() },
        }
    }

    pub const fn with_to(mut self, to: Address) -> Self {
        self.tx.to = TxKind::Call(to);
        self
    }

    pub const fn with_create(mut self) -> Self {
        self.tx.to = TxKind::Create;
        self
    }

    pub fn with_value(mut self, value: u128) -> Self {
        self.tx.value = U256::from(value);
        self
    }

    pub fn with_signer(mut self, signer: &PrivateKeySigner) -> Self {
        self.signer = Some(signer.clone());
        self
    }

    pub const fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.tx.chain_id = chain_id;
        self
    }

    pub const fn with_nonce(mut self, nonce: u64) -> Self {
        self.tx.nonce = nonce;
        self
    }

    pub const fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.tx.gas_limit = gas_limit;
        self
    }

    pub const fn with_max_fee_per_gas(mut self, max_fee_per_gas: u128) -> Self {
        self.tx.max_fee_per_gas = max_fee_per_gas;
        self
    }

    pub const fn with_max_priority_fee_per_gas(mut self, max_priority_fee_per_gas: u128) -> Self {
        self.tx.max_priority_fee_per_gas = max_priority_fee_per_gas;
        self
    }

    pub fn with_input(mut self, input: Bytes) -> Self {
        self.tx.input = input;
        self
    }

    pub fn with_revert(mut self) -> Self {
        self.tx.input = hex!("60006000fd").into();
        self
    }

    pub async fn build(mut self) -> Recovered<OpTxEnvelope> {
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

        sign_op_tx(&signer, OpTypedTransaction::Eip1559(self.tx))
            .expect("Failed to sign transaction")
    }

    pub async fn send(self) -> eyre::Result<PendingTransactionBuilder<Base>> {
        let provider = self.provider.clone();
        let transaction = self.build().await;
        let transaction_encoded = transaction.encoded_2718();

        Ok(provider.send_raw_transaction(transaction_encoded.as_slice()).await?)
    }
}

type ObservationsMap = DashMap<TxHash, VecDeque<TransactionEvent>>;

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
    pub fn new(stream: AllTransactionsEvents<OpPooledTransaction>) -> Self {
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
                                tracing::debug!("Transaction pending: {hash}");
                                observations.entry(hash).or_default().push_back(TransactionEvent::Pending);
                            },
                            Some(FullTransactionEvent::Queued(hash, _)) => {
                                tracing::debug!("Transaction queued: {hash}");
                                observations.entry(hash).or_default().push_back(TransactionEvent::Queued);
                            },
                            Some(FullTransactionEvent::Mined { tx_hash, block_hash }) => {
                                tracing::debug!("Transaction mined: {tx_hash} in block {block_hash}");
                                observations.entry(tx_hash).or_default().push_back(TransactionEvent::Mined(block_hash));
                            },
                            Some(FullTransactionEvent::Replaced { transaction, replaced_by }) => {
                                tracing::debug!("Transaction replaced: {transaction:?} by {replaced_by}");
                                observations.entry(*transaction.hash()).or_default().push_back(TransactionEvent::Replaced(replaced_by));
                            },
                            Some(FullTransactionEvent::Discarded(hash)) => {
                                tracing::debug!("Transaction discarded: {hash}");
                                observations.entry(hash).or_default().push_back(TransactionEvent::Discarded);
                            },
                            Some(FullTransactionEvent::Invalid(hash)) => {
                                tracing::debug!("Transaction invalid: {hash}");
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

    pub fn tx_status(&self, txhash: TxHash) -> Option<TransactionEvent> {
        self.observations.get(&txhash).and_then(|history| history.back().cloned())
    }

    pub fn is_pending(&self, txhash: TxHash) -> bool {
        matches!(self.tx_status(txhash), Some(TransactionEvent::Pending))
    }

    pub fn is_queued(&self, txhash: TxHash) -> bool {
        matches!(self.tx_status(txhash), Some(TransactionEvent::Queued))
    }

    pub fn is_dropped(&self, txhash: TxHash) -> bool {
        matches!(self.tx_status(txhash), Some(TransactionEvent::Discarded))
    }

    pub fn count(&self, status: TransactionEvent) -> usize {
        self.observations.iter().filter(|tx| tx.value().back() == Some(&status)).count()
    }

    pub fn pending_count(&self) -> usize {
        self.count(TransactionEvent::Pending)
    }

    pub fn queued_count(&self) -> usize {
        self.count(TransactionEvent::Queued)
    }

    pub fn dropped_count(&self) -> usize {
        self.count(TransactionEvent::Discarded)
    }

    /// Returns the history of pool events for a transaction.
    pub fn history(&self, txhash: TxHash) -> Option<Vec<TransactionEvent>> {
        self.observations.get(&txhash).map(|history| history.iter().cloned().collect())
    }

    pub fn print_all(&self) {
        tracing::debug!("TxPool {:#?}", self.observations);
    }

    pub fn exists(&self, txhash: TxHash) -> bool {
        matches!(
            self.tx_status(txhash),
            Some(TransactionEvent::Pending) | Some(TransactionEvent::Queued)
        )
    }
}
