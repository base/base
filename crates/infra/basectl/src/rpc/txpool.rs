//! Transaction-pool RPC fetch helpers and normalized report types.

use std::{collections::BTreeMap, time::Duration};

use alloy_consensus::Transaction as ConsensusTransaction;
use alloy_primitives::{Address, TxHash};
use alloy_provider::{
    Network, Provider, ProviderBuilder, ext::TxPoolApi, network::TransactionResponse,
};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_txpool::{TxpoolContent, TxpoolContentFrom};
use alloy_transport::TransportError;
use alloy_transport_http::Http;
use base_common_network::Base;
use jsonrpsee::{
    core::client::{ClientT, Error as JsonRpcClientError},
    http_client::HttpClientBuilder,
    rpc_params,
};
use serde::Serialize;
use url::Url;

use crate::errors::TxpoolCommandError;

/// Full txpool content shape for Base transaction responses.
pub type BaseTxpoolContent = TxpoolContent<<Base as Network>::TransactionResponse>;

/// Sender-filtered txpool content shape for Base transaction responses.
pub type BaseTxpoolContentFrom = TxpoolContentFrom<<Base as Network>::TransactionResponse>;

/// Txpool read scope selected by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TxpoolScope {
    /// Include pending transactions.
    Pending,
    /// Include queued transactions.
    Queued,
    /// Include pending and queued transactions.
    All,
}

impl TxpoolScope {
    /// Returns the CLI label for this txpool scope.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Queued => "queued",
            Self::All => "all",
        }
    }

    /// Returns whether this scope includes pending transactions.
    pub const fn includes_pending(self) -> bool {
        matches!(self, Self::Pending | Self::All)
    }

    /// Returns whether this scope includes queued transactions.
    pub const fn includes_queued(self) -> bool {
        matches!(self, Self::Queued | Self::All)
    }

    /// Removes out-of-scope entries while preserving the `txpool_content` wire shape.
    pub fn filter_content<T>(self, content: &mut TxpoolContent<T>) {
        if !self.includes_pending() {
            content.pending.clear();
        }
        if !self.includes_queued() {
            content.queued.clear();
        }
    }

    /// Removes out-of-scope entries while preserving the `txpool_contentFrom` wire shape.
    pub fn filter_content_from<T>(self, content: &mut TxpoolContentFrom<T>) {
        if !self.includes_pending() {
            content.pending.clear();
        }
        if !self.includes_queued() {
            content.queued.clear();
        }
    }
}

/// Pool location for a single transaction row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TxpoolTransactionPool {
    /// Transaction is pending for inclusion.
    Pending,
    /// Transaction is queued for future execution.
    Queued,
}

impl TxpoolTransactionPool {
    /// Returns the display label for this pool location.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Queued => "queued",
        }
    }
}

/// Transaction counts grouped by pool state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxpoolCounts {
    /// Pending transaction count.
    pub pending: usize,
    /// Queued transaction count.
    pub queued: usize,
    /// Pending plus queued transaction count.
    pub total: usize,
}

impl TxpoolCounts {
    /// Builds a count object from pending and queued counts.
    pub const fn new(pending: usize, queued: usize) -> Self {
        Self { pending, queued, total: pending + queued }
    }
}

/// Per-sender txpool summary for pretty and JSON output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxpoolSenderSummary {
    /// Sender address.
    pub sender: Address,
    /// Pending transaction count for this sender.
    pub pending: usize,
    /// Queued transaction count for this sender.
    pub queued: usize,
    /// Pending plus queued transaction count for this sender.
    pub total: usize,
    /// Lowest decoded nonce seen for this sender.
    pub lowest_nonce: Option<u64>,
    /// Highest decoded nonce seen for this sender.
    pub highest_nonce: Option<u64>,
}

impl TxpoolSenderSummary {
    /// Builds a zero-count summary for a sender with no matching transactions.
    pub const fn empty(sender: Address) -> Self {
        Self { sender, pending: 0, queued: 0, total: 0, lowest_nonce: None, highest_nonce: None }
    }
}

/// Humanized transaction row decoded from txpool wire content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxpoolTransactionRow {
    /// Pool location for this transaction.
    pub pool: TxpoolTransactionPool,
    /// Sender address.
    pub sender: Address,
    /// Nonce decoded from the transaction payload.
    pub nonce: u64,
    /// Original nonce map key from the txpool response.
    pub nonce_key: String,
    /// Transaction hash.
    pub hash: TxHash,
    /// Transaction type byte.
    pub tx_type: u8,
    /// Destination address, or `None` for contract creation.
    pub to: Option<Address>,
    /// Transaction value in wei.
    pub value_wei: String,
    /// Gas limit.
    pub gas_limit: u64,
    /// Legacy gas price in wei, when available.
    pub gas_price_wei: Option<u128>,
    /// EIP-1559 max fee per gas in wei.
    pub max_fee_per_gas_wei: u128,
    /// EIP-1559 max priority fee per gas in wei, when available.
    pub max_priority_fee_per_gas_wei: Option<u128>,
    /// Input data byte length.
    pub input_bytes: usize,
}

impl TxpoolTransactionRow {
    /// Builds a humanized row from a transaction response.
    pub fn from_transaction<T>(
        pool: TxpoolTransactionPool,
        sender: Address,
        nonce_key: String,
        transaction: &T,
    ) -> Self
    where
        T: ConsensusTransaction + TransactionResponse,
    {
        Self {
            pool,
            sender,
            nonce: transaction.nonce(),
            nonce_key,
            hash: transaction.tx_hash(),
            tx_type: transaction.ty(),
            to: transaction.to(),
            value_wei: transaction.value().to_string(),
            gas_limit: transaction.gas_limit(),
            gas_price_wei: ConsensusTransaction::gas_price(transaction),
            max_fee_per_gas_wei: ConsensusTransaction::max_fee_per_gas(transaction),
            max_priority_fee_per_gas_wei: ConsensusTransaction::max_priority_fee_per_gas(
                transaction,
            ),
            input_bytes: transaction.input().len(),
        }
    }
}

/// Normalized txpool report shared by full-pool and sender-filtered reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxpoolReport {
    /// Scope selected for this report.
    pub scope: TxpoolScope,
    /// Optional sender filter applied at the RPC layer.
    pub sender: Option<Address>,
    /// Included transaction counts.
    pub counts: TxpoolCounts,
    /// Per-sender summaries over the included transactions.
    pub senders: Vec<TxpoolSenderSummary>,
    /// Decoded transaction rows over the included transactions.
    pub transactions: Vec<TxpoolTransactionRow>,
}

impl TxpoolReport {
    /// Normalizes an unfiltered `txpool_content` response.
    pub fn from_content<T>(scope: TxpoolScope, content: &TxpoolContent<T>) -> Self
    where
        T: ConsensusTransaction + TransactionResponse,
    {
        let mut rows = Vec::new();
        if scope.includes_pending() {
            rows.extend(TxpoolClient::rows_from_sender_maps(
                TxpoolTransactionPool::Pending,
                &content.pending,
            ));
        }
        if scope.includes_queued() {
            rows.extend(TxpoolClient::rows_from_sender_maps(
                TxpoolTransactionPool::Queued,
                &content.queued,
            ));
        }
        Self::from_rows(scope, None, rows)
    }

    /// Normalizes a sender-filtered `txpool_contentFrom` response.
    pub fn from_content_from<T>(
        scope: TxpoolScope,
        sender: Address,
        content: &TxpoolContentFrom<T>,
    ) -> Self
    where
        T: ConsensusTransaction + TransactionResponse,
    {
        let mut rows = Vec::new();
        if scope.includes_pending() {
            rows.extend(TxpoolClient::rows_from_nonce_map(
                TxpoolTransactionPool::Pending,
                sender,
                &content.pending,
            ));
        }
        if scope.includes_queued() {
            rows.extend(TxpoolClient::rows_from_nonce_map(
                TxpoolTransactionPool::Queued,
                sender,
                &content.queued,
            ));
        }
        Self::from_rows(scope, Some(sender), rows)
    }

    /// Builds a normalized report from pre-decoded rows.
    pub fn from_rows(
        scope: TxpoolScope,
        sender: Option<Address>,
        mut transactions: Vec<TxpoolTransactionRow>,
    ) -> Self {
        transactions.sort_by(|a, b| {
            (a.pool, a.sender, a.nonce, &a.nonce_key, a.hash).cmp(&(
                b.pool,
                b.sender,
                b.nonce,
                &b.nonce_key,
                b.hash,
            ))
        });

        let pending =
            transactions.iter().filter(|tx| tx.pool == TxpoolTransactionPool::Pending).count();
        let queued =
            transactions.iter().filter(|tx| tx.pool == TxpoolTransactionPool::Queued).count();
        let counts = TxpoolCounts::new(pending, queued);
        let senders = TxpoolClient::summarize_senders(sender, &transactions);

        Self { scope, sender, counts, senders, transactions }
    }
}

/// Transaction-pool RPC client helpers.
#[derive(Debug)]
pub struct TxpoolClient;

impl TxpoolClient {
    /// Fetches raw txpool content via `txpool_content`.
    pub async fn fetch_txpool_content(rpc: &Url) -> Result<BaseTxpoolContent, TxpoolCommandError> {
        const METHOD: &str = "txpool_content";

        Self::connect_txpool_provider(rpc)?
            .txpool_content()
            .await
            .map_err(|error| Self::txpool_transport_error(rpc, METHOD, error))
    }

    /// Fetches raw sender-filtered txpool content via `txpool_contentFrom`.
    pub async fn fetch_txpool_content_from(
        rpc: &Url,
        sender: Address,
    ) -> Result<BaseTxpoolContentFrom, TxpoolCommandError> {
        const METHOD: &str = "txpool_contentFrom";

        Self::connect_txpool_provider(rpc)?
            .txpool_content_from(sender)
            .await
            .map_err(|error| Self::txpool_transport_error(rpc, METHOD, error))
    }

    /// Fetches and normalizes txpool content for a read command.
    pub async fn fetch_txpool_report(
        rpc: &Url,
        scope: TxpoolScope,
        sender: Option<Address>,
    ) -> Result<TxpoolReport, TxpoolCommandError> {
        match sender {
            Some(sender) => {
                let content = Self::fetch_txpool_content_from(rpc, sender).await?;
                Ok(TxpoolReport::from_content_from(scope, sender, &content))
            }
            None => {
                let content = Self::fetch_txpool_content(rpc).await?;
                Ok(TxpoolReport::from_content(scope, &content))
            }
        }
    }

    /// Clears the full transaction pool via upstream Reth `admin_clearTxpool`.
    pub async fn clear_txpool(rpc: &Url) -> Result<u64, TxpoolCommandError> {
        const METHOD: &str = "admin_clearTxpool";

        let client = Self::connect_admin_client(rpc)?;
        ClientT::request(&client, METHOD, rpc_params![])
            .await
            .map_err(|error| Self::admin_jsonrpc_error(rpc, METHOD, error))
    }

    /// Drops every txpool transaction for one sender via Base `admin_dropSenderTransactions`.
    pub async fn drop_sender_transactions(
        rpc: &Url,
        sender: Address,
    ) -> Result<Vec<TxHash>, TxpoolCommandError> {
        const METHOD: &str = "admin_dropSenderTransactions";

        let client = Self::connect_admin_client(rpc)?;
        ClientT::request(&client, METHOD, rpc_params![sender])
            .await
            .map_err(|error| Self::admin_jsonrpc_error(rpc, METHOD, error))
    }

    fn connect_txpool_provider(rpc: &Url) -> Result<impl Provider<Base>, TxpoolCommandError> {
        let http_client = alloy_transport_http::reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| TxpoolCommandError::BuildHttpClient {
                rpc: rpc.to_string(),
                message: error.to_string(),
            })?;
        let transport = Http::with_client(http_client, rpc.clone());
        Ok(ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_client(RpcClient::new(transport, false)))
    }

    fn connect_admin_client(
        rpc: &Url,
    ) -> Result<jsonrpsee::http_client::HttpClient, TxpoolCommandError> {
        HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(30))
            .build(rpc.as_str())
            .map_err(|error| TxpoolCommandError::BuildAdminClient {
                rpc: rpc.to_string(),
                message: error.to_string(),
            })
    }

    fn txpool_transport_error(
        rpc: &Url,
        method: &'static str,
        error: TransportError,
    ) -> TxpoolCommandError {
        if Self::is_transport_method_not_found(&error) {
            TxpoolCommandError::TxpoolMethodUnavailable { rpc: rpc.to_string(), method }
        } else {
            TxpoolCommandError::TxpoolRpc {
                rpc: rpc.to_string(),
                method,
                message: error.to_string(),
            }
        }
    }

    fn admin_jsonrpc_error(
        rpc: &Url,
        method: &'static str,
        error: JsonRpcClientError,
    ) -> TxpoolCommandError {
        if Self::is_jsonrpc_method_not_found(&error) {
            TxpoolCommandError::AdminMethodUnavailable { rpc: rpc.to_string(), method }
        } else {
            TxpoolCommandError::AdminRpc {
                rpc: rpc.to_string(),
                method,
                message: error.to_string(),
            }
        }
    }

    const fn is_transport_method_not_found(error: &TransportError) -> bool {
        matches!(error, TransportError::ErrorResp(payload) if payload.code == -32601)
    }

    fn is_jsonrpc_method_not_found(error: &JsonRpcClientError) -> bool {
        matches!(error, JsonRpcClientError::Call(payload) if payload.code() == -32601)
    }

    fn rows_from_sender_maps<T>(
        pool: TxpoolTransactionPool,
        sender_maps: &BTreeMap<Address, BTreeMap<String, T>>,
    ) -> Vec<TxpoolTransactionRow>
    where
        T: ConsensusTransaction + TransactionResponse,
    {
        sender_maps
            .iter()
            .flat_map(|(sender, nonce_map)| Self::rows_from_nonce_map(pool, *sender, nonce_map))
            .collect()
    }

    fn rows_from_nonce_map<T>(
        pool: TxpoolTransactionPool,
        sender: Address,
        nonce_map: &BTreeMap<String, T>,
    ) -> Vec<TxpoolTransactionRow>
    where
        T: ConsensusTransaction + TransactionResponse,
    {
        nonce_map
            .iter()
            .map(|(nonce_key, transaction)| {
                TxpoolTransactionRow::from_transaction(pool, sender, nonce_key.clone(), transaction)
            })
            .collect()
    }

    fn summarize_senders(
        selected_sender: Option<Address>,
        transactions: &[TxpoolTransactionRow],
    ) -> Vec<TxpoolSenderSummary> {
        let mut summaries = BTreeMap::<Address, TxpoolSenderSummary>::new();
        for tx in transactions {
            let summary =
                summaries.entry(tx.sender).or_insert_with(|| TxpoolSenderSummary::empty(tx.sender));
            match tx.pool {
                TxpoolTransactionPool::Pending => summary.pending += 1,
                TxpoolTransactionPool::Queued => summary.queued += 1,
            }
            summary.total += 1;
            summary.lowest_nonce =
                Some(summary.lowest_nonce.map_or(tx.nonce, |nonce| nonce.min(tx.nonce)));
            summary.highest_nonce =
                Some(summary.highest_nonce.map_or(tx.nonce, |nonce| nonce.max(tx.nonce)));
        }
        if let Some(sender) = selected_sender {
            summaries.entry(sender).or_insert_with(|| TxpoolSenderSummary::empty(sender));
        }
        summaries.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::{Address, B256, address};
    use alloy_provider::Network;
    use alloy_rpc_types_txpool::{TxpoolContent, TxpoolContentFrom};
    use base_common_network::Base;
    use serde_json::json;

    use super::{TxpoolReport, TxpoolScope, TxpoolTransactionPool};

    type TestTx = <Base as Network>::TransactionResponse;

    fn test_tx(sender: Address, nonce: u64, hash: B256) -> TestTx {
        serde_json::from_value(json!({
            "type": "0x2",
            "chainId": "0x2105",
            "nonce": format!("0x{nonce:x}"),
            "gas": "0x5208",
            "gasPrice": "0x3b9aca00",
            "maxFeePerGas": "0x3b9aca00",
            "maxPriorityFeePerGas": "0x5f5e100",
            "accessList": [],
            "to": "0x2222222222222222222222222222222222222222",
            "value": "0x7b",
            "input": "0x1234",
            "r": "0x1",
            "s": "0x2",
            "yParity": "0x1",
            "v": "0x1",
            "hash": hash,
            "from": sender,
            "blockHash": null,
            "blockNumber": null,
            "transactionIndex": null
        }))
        .expect("valid transaction")
    }

    #[test]
    fn normalizes_full_pool_by_scope() {
        let sender_a = address!("1111111111111111111111111111111111111111");
        let sender_b = address!("2222222222222222222222222222222222222222");
        let mut content = TxpoolContent::<TestTx>::default();
        content
            .pending
            .entry(sender_a)
            .or_default()
            .insert("1".to_string(), test_tx(sender_a, 1, B256::repeat_byte(0x01)));
        content
            .queued
            .entry(sender_a)
            .or_default()
            .insert("2".to_string(), test_tx(sender_a, 2, B256::repeat_byte(0x02)));
        content
            .queued
            .entry(sender_b)
            .or_default()
            .insert("7".to_string(), test_tx(sender_b, 7, B256::repeat_byte(0x03)));

        let pending = TxpoolReport::from_content(TxpoolScope::Pending, &content);
        assert_eq!(pending.counts.pending, 1);
        assert_eq!(pending.counts.queued, 0);
        assert_eq!(pending.transactions[0].pool, TxpoolTransactionPool::Pending);
        assert_eq!(pending.senders.len(), 1);
        assert_eq!(pending.senders[0].sender, sender_a);

        let all = TxpoolReport::from_content(TxpoolScope::All, &content);
        assert_eq!(all.counts.total, 3);
        assert_eq!(all.senders.len(), 2);
        assert_eq!(all.senders[0].total, 2);
        assert_eq!(all.senders[0].lowest_nonce, Some(1));
        assert_eq!(all.senders[0].highest_nonce, Some(2));
    }

    #[test]
    fn normalizes_sender_filtered_pool() {
        let sender = address!("1111111111111111111111111111111111111111");
        let mut content =
            TxpoolContentFrom::<TestTx> { pending: BTreeMap::new(), queued: BTreeMap::new() };
        content.pending.insert("3".to_string(), test_tx(sender, 3, B256::repeat_byte(0x03)));
        content.queued.insert("4".to_string(), test_tx(sender, 4, B256::repeat_byte(0x04)));

        let report = TxpoolReport::from_content_from(TxpoolScope::Queued, sender, &content);

        assert_eq!(report.sender, Some(sender));
        assert_eq!(report.counts.pending, 0);
        assert_eq!(report.counts.queued, 1);
        assert_eq!(report.senders.len(), 1);
        assert_eq!(report.senders[0].sender, sender);
        assert_eq!(report.transactions[0].nonce, 4);
    }

    #[test]
    fn sender_filtered_empty_pool_keeps_sender_context() {
        let sender = address!("1111111111111111111111111111111111111111");
        let content = TxpoolContentFrom::<TestTx>::default();

        let report = TxpoolReport::from_content_from(TxpoolScope::All, sender, &content);

        assert_eq!(report.counts.total, 0);
        assert_eq!(report.senders.len(), 1);
        assert_eq!(report.senders[0].sender, sender);
        assert_eq!(report.senders[0].total, 0);
    }
}
