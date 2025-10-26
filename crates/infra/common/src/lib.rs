use alloy_consensus::Transaction;
use alloy_consensus::transaction::SignerRecoverable;
use alloy_primitives::{Address, TxHash};
use alloy_provider::network::eip2718::Decodable2718;
use alloy_rpc_types_mev::EthSendBundle;
use anyhow::Result;
use chrono::{DateTime, Utc};
use op_alloy_consensus::OpTxEnvelope;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "bundle_state", rename_all = "PascalCase")]
pub enum BundleState {
    Ready,
    IncludedByBuilder,
}

/// Extended bundle data that includes the original bundle plus extracted metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleWithMetadata {
    pub bundle: EthSendBundle,
    pub txn_hashes: Vec<TxHash>,
    pub senders: Vec<Address>,
    pub min_base_fee: i64,
    pub state: BundleState,
    pub state_changed_at: DateTime<Utc>,
}

impl BundleWithMetadata {
    pub fn new(bundle: &EthSendBundle) -> Result<Self> {
        let mut senders = Vec::new();
        let mut txn_hashes = Vec::new();

        let mut min_base_fee = i64::MAX;

        for tx_bytes in &bundle.txs {
            let envelope = OpTxEnvelope::decode_2718_exact(tx_bytes)?;
            txn_hashes.push(*envelope.hash());

            let sender = match envelope.recover_signer() {
                Ok(signer) => signer,
                Err(err) => return Err(err.into()),
            };

            senders.push(sender);
            min_base_fee = min_base_fee.min(envelope.max_fee_per_gas() as i64); // todo type and todo not right
        }

        let minimum_base_fee = if min_base_fee == i64::MAX {
            0
        } else {
            min_base_fee
        };

        Ok(Self {
            bundle: bundle.clone(),
            txn_hashes,
            senders,
            min_base_fee: minimum_base_fee,
            state: BundleState::Ready,
            state_changed_at: Utc::now(),
        })
    }
}
