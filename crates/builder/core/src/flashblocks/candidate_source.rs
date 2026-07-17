//! Pool candidate sourcing for the flashblock build loop.
//!
//! [`PoolCandidateSource`] wraps the payload transaction iterator and yields raw, fully
//! materialized [`Candidate`]s. It makes no admission decisions — bundle-window validity travels
//! on the candidate as a [`BundleWindow`] for the gates to judge — so the build walk owns the gate
//! sequence and the source owns only iteration and materialization.

use alloy_consensus::Transaction;
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256, TxHash};
use base_bundles::MeterBundleResponse;
use base_common_consensus::BaseTransactionSigned;
use base_execution_txpool::{
    BasePooledTx, BundleTransaction, TimestampedTransaction, WatchManifest,
    estimated_da_size::DataAvailabilitySized,
};
use reth_primitives_traits::Recovered;
use reth_transaction_pool::PoolTransaction;

use crate::{PayloadTxsBounds, TxResources};

/// A bundle transaction's validity window, carried on a [`Candidate`] so the gate can evaluate a
/// materialized candidate without retaining the pool transaction.
///
/// Implements [`BundleTransaction`] purely so it reuses that trait's default `is_bundle_expired` /
/// `is_bundle_not_yet_valid` predicates — the single source of truth for bundle-window logic.
#[derive(Debug, Clone, Copy, Default)]
pub struct BundleWindow {
    /// The target block number, if set.
    pub target_block: Option<u64>,
    /// The minimum validity timestamp in milliseconds, if set.
    pub min_timestamp_millis: Option<u64>,
    /// The maximum validity timestamp in milliseconds, if set.
    pub max_timestamp_millis: Option<u64>,
}

impl BundleTransaction for BundleWindow {
    fn target_block_number(&self) -> Option<u64> {
        self.target_block
    }

    fn min_timestamp_millis(&self) -> Option<u64> {
        self.min_timestamp_millis
    }

    fn max_timestamp_millis(&self) -> Option<u64> {
        self.max_timestamp_millis
    }
}

/// A materialized pool candidate threaded through the admission gates.
///
/// The source populates everything except the metering-derived fields: [`resources`] starts with
/// no execution-time prediction and [`resource_usage`] is `None`. The resource-limits gate enriches
/// both from metering-service data during evaluation.
///
/// [`resources`]: Candidate::resources
/// [`resource_usage`]: Candidate::resource_usage
#[derive(Debug)]
pub struct Candidate {
    /// Recovered consensus transaction.
    pub tx: Recovered<BaseTransactionSigned>,
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Time the transaction was received, in milliseconds since the Unix epoch.
    pub received_at_ms: u128,
    /// Bundle validity window (empty for non-bundle transactions).
    pub bundle: BundleWindow,
    /// EIP-8130 authorization manifest to revalidate against on-chain state, if the transaction
    /// carries one.
    pub watch_manifest: Option<WatchManifest>,
    /// EIP-8130 replay identifier, if applicable. Nonce-free replay-ID entries are independent, so
    /// a manifest-precheck drop must not mark the sender's other entries invalid.
    pub eip8130_replay_id: Option<B256>,
    /// Effective priority fee (tip per gas), used as the value for rejection metrics.
    pub priority_fee: f64,
    /// Estimated resource usage; execution time is filled in by the resource-limits gate.
    pub resources: TxResources,
    /// Raw metering response, if any; filled in by the resource-limits gate.
    pub resource_usage: Option<MeterBundleResponse>,
}

/// Sources fully materialized candidate transactions from a payload transaction iterator.
#[derive(Debug)]
pub struct PoolCandidateSource<'a, T> {
    best_txs: &'a mut T,
    base_fee: u64,
}

impl<'a, T: PayloadTxsBounds> PoolCandidateSource<'a, T> {
    /// Creates a source over the given iterator, using `base_fee` to compute each candidate's
    /// priority fee.
    pub const fn new(best_txs: &'a mut T, base_fee: u64) -> Self {
        Self { best_txs, base_fee }
    }

    /// Returns the next fully materialized candidate, or `None` when the pool is drained.
    pub fn next_candidate(&mut self) -> Option<Candidate> {
        let tx = self.best_txs.next(())?;

        let bundle = BundleWindow {
            target_block: tx.target_block_number(),
            min_timestamp_millis: tx.min_timestamp_millis(),
            max_timestamp_millis: tx.max_timestamp_millis(),
        };
        let da_size = tx.estimated_da_size();
        let received_at_ms = tx.received_at();
        // Capture the EIP-8130 manifest and replay id before `into_consensus` drops the pool tx.
        let watch_manifest = tx.watch_manifest().cloned();
        let eip8130_replay_id = tx.eip8130_replay_id();
        let tx = tx.into_consensus();
        let tx_hash = tx.tx_hash();
        let uncompressed_size = tx.encode_2718_len() as u64;
        let gas_limit = tx.gas_limit();
        let priority_fee = tx.effective_tip_per_gas(self.base_fee).unwrap_or(0) as f64;

        Some(Candidate {
            tx,
            tx_hash,
            received_at_ms,
            bundle,
            watch_manifest,
            eip8130_replay_id,
            priority_fee,
            resources: TxResources {
                da_size,
                gas_limit,
                execution_time_us: None,
                uncompressed_size,
            },
            resource_usage: None,
        })
    }

    /// Marks a sender/nonce as invalid on the underlying iterator so its descendants are skipped.
    pub fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.best_txs.mark_invalid(sender, nonce);
    }
}
