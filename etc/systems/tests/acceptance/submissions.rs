//! Observes successful L1 inbox submissions and attributes decoded channels to test traffic.

use std::time::Duration;

use alloy_consensus::proofs::calculate_transaction_root;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_eth::Transaction;
use base_common_genesis::RollupConfig;
use base_protocol::BlockInfo;
use base_system_tests::BATCHER;
use eyre::{Result, WrapErr, ensure};
use serde_json::json;
use tokio::time::{sleep, timeout};

use super::{
    activation::Schedule,
    batch::{BatchAttribution, BatchObserver, Submission},
    blob::BlobEvidence,
    header::AuthenticatedHeader,
    rpc::Rpc,
    transfer::Transfer,
};

/// A chronological scan of canonical L1 blocks, with production channel reassembly.
#[derive(Debug)]
pub struct Submissions {
    /// First L1 block not yet inspected.
    pub next_block: u64,
    /// Partial channels carried across successive L1 blocks.
    pub observer: BatchObserver,
    /// Real consensus endpoint serving hash-authenticated blob data.
    pub beacon: String,
    /// Consensus genesis timestamp used to map L1 inclusion to its beacon slot.
    pub genesis: u64,
    /// Consensus slot duration from the matched live schedule.
    pub seconds_per_slot: u64,
}

impl Submissions {
    /// Starts at a known L1 head from before the transfer was sent.
    pub fn new(first_block: u64, beacon: String, schedule: &Schedule) -> Self {
        Self {
            next_block: first_block,
            observer: BatchObserver::default(),
            beacon,
            genesis: schedule.genesis,
            seconds_per_slot: schedule.seconds_per_slot,
        }
    }

    /// Rechecks contributing L1 blocks and receipts after safe derivation and consensus finality.
    pub async fn require_canonical(
        rpc: &Rpc,
        l1: &str,
        attribution: &BatchAttribution,
    ) -> Result<()> {
        for submission in &attribution.submissions {
            let header = rpc.header(l1, json!(format!("{:#x}", submission.block.number))).await?;
            ensure!(header.hash == submission.block.hash, "batch inclusion block was replaced");
            let receipt = rpc
                .call(l1, "eth_getTransactionReceipt", json!([submission.transaction["hash"]]))
                .await?;
            ensure!(receipt == submission.receipt, "batch receipt changed after safe derivation");
        }
        Ok(())
    }

    /// Waits for a complete channel containing exactly the receipt-anchored transfer.
    pub async fn wait_for_transfer(
        &mut self,
        rpc: &Rpc,
        l1: &str,
        rollup: &RollupConfig,
        transfer: &Transfer,
    ) -> Result<BatchAttribution> {
        let target = transfer.target();
        let result = timeout(Duration::from_secs(120), async {
            loop {
                let head = rpc.header(l1, json!("latest")).await?;
                while self.next_block <= head.header.number {
                    let raw = rpc
                        .call(
                            l1,
                            "eth_getBlockByNumber",
                            json!([format!("{:#x}", self.next_block), true]),
                        )
                        .await?;
                    let header = AuthenticatedHeader::parse(raw.clone())?;
                    let transactions: Vec<Transaction> =
                        serde_json::from_value(raw["transactions"].clone())?;
                    let transactions = transactions
                        .into_iter()
                        .map(|transaction| transaction.inner.into_inner())
                        .collect::<Vec<_>>();
                    ensure!(
                        calculate_transaction_root(&transactions)
                            == header.header.transactions_root,
                        "L1 transaction body does not match its authenticated root"
                    );
                    let mut found = None;
                    for transaction in raw["transactions"]
                        .as_array()
                        .ok_or_else(|| eyre::eyre!("L1 block has no full transactions"))?
                    {
                        if transaction["from"] != json!(BATCHER.address)
                            || transaction["to"] != json!(rollup.batch_inbox_address)
                        {
                            continue;
                        }
                        ensure!(
                            Rpc::quantity(&transaction["type"])? == 3,
                            "inbox transaction is not an EIP-4844 blob transaction: {transaction}"
                        );
                        let receipt = rpc
                            .call(l1, "eth_getTransactionReceipt", json!([transaction["hash"]]))
                            .await?;
                        ensure!(
                            Rpc::quantity(&receipt["status"])? == 1,
                            "batch submission reverted: {receipt}"
                        );
                        ensure!(
                            receipt["blockHash"] == json!(header.hash)
                                && Rpc::quantity(&receipt["blockNumber"])? == header.header.number
                                && receipt["transactionHash"] == transaction["hash"],
                            "batch receipt/header mismatch"
                        );
                        let input: Bytes = serde_json::from_value(transaction["input"].clone())?;
                        ensure!(input.is_empty(), "blob batch unexpectedly carries calldata");
                        let hashes: Vec<B256> =
                            serde_json::from_value(transaction["blobVersionedHashes"].clone())?;
                        let elapsed = header
                            .header
                            .timestamp
                            .checked_sub(self.genesis)
                            .ok_or_else(|| eyre::eyre!("batch header predates CL genesis"))?;
                        ensure!(
                            self.seconds_per_slot > 0 && elapsed % self.seconds_per_slot == 0,
                            "batch timestamp does not map to a consensus slot"
                        );
                        let blobs = BlobEvidence::fetch(
                            &self.beacon,
                            elapsed / self.seconds_per_slot,
                            &hashes,
                        )
                        .await?;
                        let payloads =
                            blobs.iter().map(|blob| blob.payload.clone()).collect::<Vec<_>>();
                        let submission = Submission {
                            transaction: transaction.clone(),
                            receipt,
                            block: BlockInfo::new(
                                header.hash,
                                header.header.number,
                                header.header.parent_hash,
                                header.header.timestamp,
                            ),
                        };
                        for payload in payloads {
                            if let Some(attribution) = self.observer.ingest(
                                payload,
                                submission.clone(),
                                &target,
                                rollup,
                            )? {
                                ensure!(
                                    attribution.transaction_hash == transfer.transaction_hash,
                                    "decoded batch transfer hash mismatch"
                                );
                                found = Some(attribution);
                            }
                        }
                    }
                    self.next_block += 1;
                    if let Some(attribution) = found {
                        return Ok::<_, eyre::Report>(attribution);
                    }
                }
                sleep(Duration::from_millis(500)).await;
            }
        })
        .await;
        result.wrap_err_with(|| format!("no complete L1 batch channel for transfer {} at L2 timestamp {}; scanned through L1 block {}, {} incomplete channels", transfer.transaction_hash, transfer.block.header.timestamp, self.next_block.saturating_sub(1), self.observer.channels.len()))?
    }
}
