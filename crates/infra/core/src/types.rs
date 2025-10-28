use alloy_consensus::transaction::SignerRecoverable;
use alloy_primitives::{Address, B256, Bytes, TxHash, keccak256};
use alloy_provider::network::eip2718::Decodable2718;
use op_alloy_consensus::OpTxEnvelope;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Default, Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Bundle {
    pub txs: Vec<Bytes>,

    #[serde(with = "alloy_serde::quantity")]
    pub block_number: u64,

    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub flashblock_number_min: Option<u64>,

    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub flashblock_number_max: Option<u64>,

    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub min_timestamp: Option<u64>,

    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_timestamp: Option<u64>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverting_tx_hashes: Vec<TxHash>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_uuid: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropping_tx_hashes: Vec<TxHash>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleHash {
    pub bundle_hash: B256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelBundle {
    pub replacement_uuid: String,
}

#[derive(Debug, Clone)]
pub struct BundleWithMetadata {
    bundle: Bundle,
    uuid: Uuid,
    transactions: Vec<OpTxEnvelope>,
}

impl BundleWithMetadata {
    pub fn load(mut bundle: Bundle) -> Result<Self, String> {
        let uuid = bundle
            .replacement_uuid
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let uuid = Uuid::parse_str(uuid.as_str()).map_err(|_| format!("Invalid UUID: {uuid}"))?;

        bundle.replacement_uuid = Some(uuid.to_string());

        let transactions: Vec<OpTxEnvelope> = bundle
            .txs
            .iter()
            .map(|b| {
                OpTxEnvelope::decode_2718_exact(b)
                    .map_err(|e| format!("failed to decode transaction: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(BundleWithMetadata {
            bundle,
            transactions,
            uuid,
        })
    }

    pub fn transactions(&self) -> &[OpTxEnvelope] {
        self.transactions.as_slice()
    }

    pub fn uuid(&self) -> &Uuid {
        &self.uuid
    }

    pub fn bundle_hash(&self) -> B256 {
        let mut concatenated = Vec::new();
        for tx in self.transactions() {
            concatenated.extend_from_slice(tx.tx_hash().as_slice());
        }
        keccak256(&concatenated)
    }

    pub fn txn_hashes(&self) -> Vec<TxHash> {
        self.transactions().iter().map(|t| t.tx_hash()).collect()
    }

    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    pub fn senders(&self) -> Vec<Address> {
        self.transactions()
            .iter()
            .map(|t| t.recover_signer().unwrap())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionResult {
    pub coinbase_diff: String,
    pub eth_sent_to_coinbase: String,
    pub from_address: Address,
    pub gas_fees: String,
    pub gas_price: String,
    pub gas_used: u64,
    pub to_address: Option<Address>,
    pub tx_hash: TxHash,
    pub value: String,
    pub execution_time_us: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeterBundleResponse {
    pub bundle_gas_price: String,
    pub bundle_hash: B256,
    pub coinbase_diff: String,
    pub eth_sent_to_coinbase: String,
    pub gas_fees: String,
    pub results: Vec<TransactionResult>,
    pub state_block_number: u64,
    pub total_gas_used: u64,
    pub total_execution_time_us: u128,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_transaction;
    use alloy_primitives::Keccak256;
    use alloy_provider::network::eip2718::Encodable2718;
    use alloy_signer_local::PrivateKeySigner;

    #[test]
    fn test_bundle_types() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let tx1 = create_transaction(alice.clone(), 1, bob.address());
        let tx2 = create_transaction(alice.clone(), 2, bob.address());

        let tx1_bytes = tx1.encoded_2718();
        let tx2_bytes = tx2.encoded_2718();

        let bundle = BundleWithMetadata::load(Bundle {
            replacement_uuid: None,
            txs: vec![tx1_bytes.clone().into()],
            block_number: 1,
            ..Default::default()
        })
        .unwrap();

        assert!(!bundle.uuid().is_nil());
        assert_eq!(
            bundle.bundle.replacement_uuid,
            Some(bundle.uuid().to_string())
        );
        assert_eq!(bundle.txn_hashes().len(), 1);
        assert_eq!(bundle.txn_hashes()[0], tx1.tx_hash());
        assert_eq!(bundle.senders().len(), 1);
        assert_eq!(bundle.senders()[0], alice.address());

        // Bundle hashes are keccack256(...txnHashes)
        let expected_bundle_hash_single = {
            let mut hasher = Keccak256::default();
            hasher.update(keccak256(&tx1_bytes));
            hasher.finalize()
        };

        assert_eq!(bundle.bundle_hash(), expected_bundle_hash_single);

        let uuid = Uuid::new_v4();
        let bundle = BundleWithMetadata::load(Bundle {
            replacement_uuid: Some(uuid.to_string()),
            txs: vec![tx1_bytes.clone().into(), tx2_bytes.clone().into()],
            block_number: 1,
            ..Default::default()
        })
        .unwrap();

        assert_eq!(*bundle.uuid(), uuid);
        assert_eq!(bundle.bundle.replacement_uuid, Some(uuid.to_string()));
        assert_eq!(bundle.txn_hashes().len(), 2);
        assert_eq!(bundle.txn_hashes()[0], tx1.tx_hash());
        assert_eq!(bundle.txn_hashes()[1], tx2.tx_hash());
        assert_eq!(bundle.senders().len(), 2);
        assert_eq!(bundle.senders()[0], alice.address());
        assert_eq!(bundle.senders()[1], alice.address());

        let expected_bundle_hash_double = {
            let mut hasher = Keccak256::default();
            hasher.update(keccak256(&tx1_bytes));
            hasher.update(keccak256(&tx2_bytes));
            hasher.finalize()
        };

        assert_eq!(bundle.bundle_hash(), expected_bundle_hash_double);
    }
}
