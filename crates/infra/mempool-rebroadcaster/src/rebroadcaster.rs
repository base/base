use std::{collections::HashMap, error::Error};

use alloy_consensus::Transaction;
use alloy_eips::{
    Encodable2718,
    eip2718::{EIP1559_TX_TYPE_ID, EIP2930_TX_TYPE_ID, EIP7702_TX_TYPE_ID, LEGACY_TX_TYPE_ID},
};
use alloy_primitives::B256;
use alloy_provider::{Provider, ProviderBuilder, RootProvider, ext::TxPoolApi};
use alloy_rpc_types::txpool::TxpoolContent;
use alloy_rpc_types_eth::{BlockId, Transaction as RpcTransaction};
use tracing::{debug, error, info, warn};

const IGNORED_ERRORS: [&str; 3] =
    ["transaction underpriced", "replacement transaction underpriced", "already known"];

/// Synchronizes transaction pools between geth and reth nodes by rebroadcasting
/// transactions that exist in one mempool but not the other.
#[derive(Debug, Clone)]
pub struct Rebroadcaster {
    geth_provider: RootProvider,
    reth_provider: RootProvider,
}

/// Result counters from a single rebroadcast run.
#[derive(Debug, Clone)]
pub struct RebroadcasterResult {
    /// Number of transactions successfully sent from geth to reth.
    pub success_geth_to_reth: u32,
    /// Number of transactions successfully sent from reth to geth.
    pub success_reth_to_geth: u32,
    /// Number of unexpected failures when sending from geth to reth.
    pub unexpected_failed_geth_to_reth: u32,
    /// Number of unexpected failures when sending from reth to geth.
    pub unexpected_failed_reth_to_geth: u32,
}

/// The difference between two transaction pools, identifying transactions
/// present in one but missing from the other.
#[derive(Debug)]
pub struct TxpoolDiff {
    /// Transactions found in the geth mempool but absent from reth.
    pub in_geth_not_in_reth: Vec<RpcTransaction>,
    /// Transactions found in the reth mempool but absent from geth.
    pub in_reth_not_in_geth: Vec<RpcTransaction>,
}

impl Rebroadcaster {
    /// Creates a new [`Rebroadcaster`] connected to the given geth and reth HTTP endpoints.
    pub fn new(geth_endpoint: String, reth_endpoint: String) -> Self {
        let geth_provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(geth_endpoint.parse().expect("Invalid geth endpoint"));

        let reth_provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(reth_endpoint.parse().expect("Invalid reth endpoint"));

        Self { geth_provider, reth_provider }
    }

    /// Executes a single rebroadcast cycle: fetches mempool contents from both nodes,
    /// filters underpriced transactions, computes the diff, and rebroadcasts missing
    /// transactions in each direction.
    pub async fn run(&self) -> Result<RebroadcasterResult, Box<dyn Error>> {
        let (base_fee, gas_price) = self.fetch_network_fees().await?;
        let (geth_mempool_contents, reth_mempool_contents) = self.fetch_mempool_contents().await?;

        let (geth_pending_count, geth_queued_count) = self.count_txns(&geth_mempool_contents);
        let (reth_pending_count, reth_queued_count) = self.count_txns(&reth_mempool_contents);

        info!(
            geth_pending_count,
            geth_queued_count, reth_pending_count, reth_queued_count, "txn counts"
        );

        let filtered_geth_mempool_contents =
            self.filter_underpriced_txns(&geth_mempool_contents, base_fee, gas_price);
        let filtered_reth_mempool_contents =
            self.filter_underpriced_txns(&reth_mempool_contents, base_fee, gas_price);

        let (filtered_geth_pending_count, filtered_geth_queued_count) =
            self.count_txns(&filtered_geth_mempool_contents);
        let (filtered_reth_pending_count, filtered_reth_queued_count) =
            self.count_txns(&filtered_reth_mempool_contents);

        info!(
            filtered_geth_pending_count,
            filtered_geth_queued_count,
            filtered_reth_pending_count,
            filtered_reth_queued_count,
            "filtered txn counts"
        );

        let diff =
            self.compute_diff(&filtered_geth_mempool_contents, &filtered_reth_mempool_contents);

        let mut output = RebroadcasterResult {
            success_geth_to_reth: 0,
            success_reth_to_geth: 0,
            unexpected_failed_geth_to_reth: 0,
            unexpected_failed_reth_to_geth: 0,
        };

        for txn in diff.in_geth_not_in_reth {
            let hash = txn.as_recovered().hash();
            let sender = txn.as_recovered().signer().to_string();
            debug!(tx = ?hash, "broadcasting txn found in geth but not in reth");
            let result = self
                .reth_provider
                .send_raw_transaction(txn.clone().into_signed().into_encoded().encoded_bytes())
                .await;

            if let Err(e) = result {
                let err_msg = e.as_error_resp().unwrap().message.to_string();
                if !IGNORED_ERRORS.contains(&err_msg.as_str()) {
                    output.unexpected_failed_geth_to_reth += 1;
                    error!(
                        tx = ?hash,
                        error = ?err_msg,
                        from = sender,
                        "error sending txn from geth to reth"
                    );
                }
                continue;
            }

            output.success_geth_to_reth += 1;
        }

        for txn in diff.in_reth_not_in_geth {
            let hash = txn.as_recovered().hash();
            let sender = txn.as_recovered().signer().to_string();
            debug!(tx = ?hash, "broadcasting txn found in reth but not in geth");
            let result = self
                .geth_provider
                .send_raw_transaction(txn.clone().into_signed().into_encoded().encoded_bytes())
                .await;

            if let Err(e) = result {
                let err_msg = e.as_error_resp().unwrap().message.to_string();
                if !IGNORED_ERRORS.contains(&err_msg.as_str()) {
                    output.unexpected_failed_reth_to_geth += 1;
                    error!(
                        tx = ?hash,
                        error = ?err_msg,
                        from = sender,
                        "error sending txn from reth to geth"
                    );
                }
                continue;
            }

            output.success_reth_to_geth += 1;
        }

        Ok(output)
    }

    async fn fetch_network_fees(&self) -> Result<(u128, u128), Box<dyn Error>> {
        let latest_block = self
            .geth_provider
            .get_block(BlockId::latest())
            .hashes()
            .await?
            .expect("Failed to get latest block");

        let gas_price = self.geth_provider.get_gas_price().await?;
        let base_fee: u128 = latest_block.header.base_fee_per_gas.map_or(gas_price, |v| v.into());

        Ok((base_fee, gas_price))
    }

    async fn fetch_mempool_contents(
        &self,
    ) -> Result<(TxpoolContent, TxpoolContent), Box<dyn Error>> {
        let (geth_mempool_contents, reth_mempool_contents) =
            tokio::join!(self.geth_provider.txpool_content(), self.reth_provider.txpool_content(),);
        let geth_mempool_contents = geth_mempool_contents?;
        let reth_mempool_contents = reth_mempool_contents?;

        Ok((geth_mempool_contents, reth_mempool_contents))
    }

    /// Returns a copy of the given mempool contents with underpriced transactions removed.
    pub fn filter_underpriced_txns(
        &self,
        content: &TxpoolContent,
        base_fee: u128,
        gas_price: u128,
    ) -> TxpoolContent {
        let mut filtered_content = content.clone();

        for (account, nonce_txns) in &content.pending {
            for (nonce, txn) in nonce_txns {
                if self.is_underpriced(txn, base_fee, gas_price) {
                    filtered_content.pending.get_mut(account).unwrap().remove(nonce);
                }
            }

            if filtered_content.pending.get(account).unwrap().is_empty() {
                filtered_content.pending.remove(account);
            }
        }

        for (account, nonce_txns) in &content.queued {
            for (nonce, txn) in nonce_txns {
                if self.is_underpriced(txn, base_fee, gas_price) {
                    filtered_content.queued.get_mut(account).unwrap().remove(nonce);
                }
            }

            if filtered_content.queued.get(account).unwrap().is_empty() {
                filtered_content.queued.remove(account);
            }
        }

        filtered_content
    }

    fn is_underpriced(&self, txn: &dyn Transaction, base_fee: u128, gas_price: u128) -> bool {
        match txn.ty() {
            LEGACY_TX_TYPE_ID | EIP2930_TX_TYPE_ID => {
                if txn.gas_price().is_none() {
                    return true;
                }
                txn.gas_price().unwrap() < gas_price
            }
            EIP1559_TX_TYPE_ID | EIP7702_TX_TYPE_ID => {
                if txn.max_priority_fee_per_gas().is_none() {
                    return true;
                }
                txn.max_fee_per_gas() < base_fee
            }
            _ => {
                warn!(
                    tx_type = ?txn.ty(),
                    "unknown transaction type, treating as underpriced"
                );
                true
            }
        }
    }

    fn count_txns(&self, mempool: &TxpoolContent) -> (usize, usize) {
        let mut pending_count = 0;
        let mut queued_count = 0;

        for nonce_txns in mempool.pending.values() {
            pending_count += nonce_txns.len();
        }

        for nonce_txns in mempool.queued.values() {
            queued_count += nonce_txns.len();
        }

        (pending_count, queued_count)
    }

    /// Computes the symmetric difference between two mempool snapshots, returning
    /// transactions unique to each pool sorted by nonce.
    pub fn compute_diff(
        &self,
        geth_mempool: &TxpoolContent,
        reth_mempool: &TxpoolContent,
    ) -> TxpoolDiff {
        let mut diff =
            TxpoolDiff { in_geth_not_in_reth: Vec::new(), in_reth_not_in_geth: Vec::new() };

        let geth_hashes = self.txns_by_hash(geth_mempool);
        let reth_hashes = self.txns_by_hash(reth_mempool);

        for (hash, txn) in &geth_hashes {
            if !reth_hashes.contains_key(hash) {
                diff.in_geth_not_in_reth.push(txn.clone());
            }
        }

        for (hash, txn) in &reth_hashes {
            if !geth_hashes.contains_key(hash) {
                diff.in_reth_not_in_geth.push(txn.clone());
            }
        }

        diff.in_geth_not_in_reth.sort_by_key(|txn| txn.as_recovered().nonce());
        diff.in_reth_not_in_geth.sort_by_key(|txn| txn.as_recovered().nonce());

        diff
    }

    fn txns_by_hash(&self, mempool: &TxpoolContent) -> HashMap<B256, RpcTransaction> {
        let mut txns_by_hash = HashMap::new();

        for nonce_txns in mempool.pending.values() {
            for txn in nonce_txns.values() {
                txns_by_hash.insert(*txn.as_recovered().hash(), txn.clone());
            }
        }

        for nonce_txns in mempool.queued.values() {
            for txn in nonce_txns.values() {
                txns_by_hash.insert(*txn.as_recovered().hash(), txn.clone());
            }
        }

        txns_by_hash
    }
}
