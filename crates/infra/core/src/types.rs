use alloy_consensus::Transaction;
use alloy_consensus::transaction::Recovered;
use alloy_consensus::transaction::SignerRecoverable;
use alloy_primitives::{Address, B256, Bytes, TxHash, U256, keccak256};
use alloy_provider::network::eip2718::{Decodable2718, Encodable2718};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `Bundle` is the type that mirrors `EthSendBundle` and is used for the API.
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

/// `ParsedBundle` is the type that contains utility methods for the `Bundle` type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedBundle {
    pub txs: Vec<Recovered<OpTxEnvelope>>,
    pub block_number: u64,
    pub flashblock_number_min: Option<u64>,
    pub flashblock_number_max: Option<u64>,
    pub min_timestamp: Option<u64>,
    pub max_timestamp: Option<u64>,
    pub reverting_tx_hashes: Vec<TxHash>,
    pub replacement_uuid: Option<Uuid>,
    pub dropping_tx_hashes: Vec<TxHash>,
}

impl TryFrom<Bundle> for ParsedBundle {
    type Error = String;
    fn try_from(bundle: Bundle) -> Result<Self, Self::Error> {
        let txs: Vec<Recovered<OpTxEnvelope>> = bundle
            .txs
            .into_iter()
            .map(|tx| {
                OpTxEnvelope::decode_2718_exact(tx.iter().as_slice())
                    .map_err(|e| format!("Failed to decode transaction: {e:?}"))
                    .and_then(|tx| {
                        tx.try_into_recovered().map_err(|e| {
                            format!("Failed to convert transaction to recovered: {e:?}")
                        })
                    })
            })
            .collect::<Result<Vec<Recovered<OpTxEnvelope>>, String>>()?;

        let uuid = bundle
            .replacement_uuid
            .map(|x| Uuid::parse_str(x.as_ref()))
            .transpose()
            .map_err(|e| format!("Invalid UUID: {e:?}"))?;

        Ok(Self {
            txs,
            block_number: bundle.block_number,
            flashblock_number_min: bundle.flashblock_number_min,
            flashblock_number_max: bundle.flashblock_number_max,
            min_timestamp: bundle.min_timestamp,
            max_timestamp: bundle.max_timestamp,
            reverting_tx_hashes: bundle.reverting_tx_hashes,
            replacement_uuid: uuid,
            dropping_tx_hashes: bundle.dropping_tx_hashes,
        })
    }
}

impl From<AcceptedBundle> for ParsedBundle {
    fn from(accepted_bundle: AcceptedBundle) -> Self {
        Self {
            txs: accepted_bundle.txs,
            block_number: accepted_bundle.block_number,
            flashblock_number_min: accepted_bundle.flashblock_number_min,
            flashblock_number_max: accepted_bundle.flashblock_number_max,
            min_timestamp: accepted_bundle.min_timestamp,
            max_timestamp: accepted_bundle.max_timestamp,
            reverting_tx_hashes: accepted_bundle.reverting_tx_hashes,
            replacement_uuid: accepted_bundle.replacement_uuid,
            dropping_tx_hashes: accepted_bundle.dropping_tx_hashes,
        }
    }
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

/// `AcceptedBundle` is the type that is sent over the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedBundle {
    pub uuid: Uuid,

    pub txs: Vec<Recovered<OpTxEnvelope>>,

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
    pub replacement_uuid: Option<Uuid>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropping_tx_hashes: Vec<TxHash>,

    pub meter_bundle_response: MeterBundleResponse,
}

pub trait BundleTxs {
    fn transactions(&self) -> &Vec<Recovered<OpTxEnvelope>>;
}

pub trait BundleExtensions {
    fn bundle_hash(&self) -> B256;
    fn txn_hashes(&self) -> Vec<TxHash>;
    fn senders(&self) -> Vec<Address>;
    fn gas_limit(&self) -> u64;
    fn da_size(&self) -> u64;
}

impl<T: BundleTxs> BundleExtensions for T {
    fn bundle_hash(&self) -> B256 {
        let parsed = self.transactions();
        let mut concatenated = Vec::new();
        for tx in parsed {
            concatenated.extend_from_slice(tx.tx_hash().as_slice());
        }
        keccak256(&concatenated)
    }

    fn txn_hashes(&self) -> Vec<TxHash> {
        self.transactions().iter().map(|t| t.tx_hash()).collect()
    }

    fn senders(&self) -> Vec<Address> {
        self.transactions()
            .iter()
            .map(|t| t.recover_signer().unwrap())
            .collect()
    }

    fn gas_limit(&self) -> u64 {
        self.transactions().iter().map(|t| t.gas_limit()).sum()
    }

    fn da_size(&self) -> u64 {
        self.transactions()
            .iter()
            .map(|t| tx_estimated_size_fjord_bytes(&t.encoded_2718()))
            .sum()
    }
}

impl BundleTxs for ParsedBundle {
    fn transactions(&self) -> &Vec<Recovered<OpTxEnvelope>> {
        &self.txs
    }
}

impl BundleTxs for AcceptedBundle {
    fn transactions(&self) -> &Vec<Recovered<OpTxEnvelope>> {
        &self.txs
    }
}

impl AcceptedBundle {
    pub fn new(bundle: ParsedBundle, meter_bundle_response: MeterBundleResponse) -> Self {
        Self {
            uuid: bundle.replacement_uuid.unwrap_or_else(Uuid::new_v4),
            txs: bundle.txs,
            block_number: bundle.block_number,
            flashblock_number_min: bundle.flashblock_number_min,
            flashblock_number_max: bundle.flashblock_number_max,
            min_timestamp: bundle.min_timestamp,
            max_timestamp: bundle.max_timestamp,
            reverting_tx_hashes: bundle.reverting_tx_hashes,
            replacement_uuid: bundle.replacement_uuid,
            dropping_tx_hashes: bundle.dropping_tx_hashes,
            meter_bundle_response,
        }
    }

    pub const fn uuid(&self) -> &Uuid {
        &self.uuid
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TransactionResult {
    pub coinbase_diff: U256,
    pub eth_sent_to_coinbase: U256,
    pub from_address: Address,
    pub gas_fees: U256,
    pub gas_price: U256,
    pub gas_used: u64,
    pub to_address: Option<Address>,
    pub tx_hash: TxHash,
    pub value: U256,
    pub execution_time_us: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MeterBundleResponse {
    pub bundle_gas_price: U256,
    pub bundle_hash: B256,
    pub coinbase_diff: U256,
    pub eth_sent_to_coinbase: U256,
    pub gas_fees: U256,
    pub results: Vec<TransactionResult>,
    pub state_block_number: u64,
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub state_flashblock_index: Option<u64>,
    pub total_gas_used: u64,
    pub total_execution_time_us: u128,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{create_test_meter_bundle_response, create_transaction};
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

        let bundle = AcceptedBundle::new(
            Bundle {
                txs: vec![tx1_bytes.clone().into()],
                block_number: 1,
                replacement_uuid: None,
                ..Default::default()
            }
            .try_into()
            .unwrap(),
            create_test_meter_bundle_response(),
        );

        assert!(!bundle.uuid().is_nil());
        assert_eq!(bundle.replacement_uuid, None); // we're fine with bundles that don't have a replacement UUID
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
        let bundle = AcceptedBundle::new(
            Bundle {
                txs: vec![tx1_bytes.clone().into(), tx2_bytes.clone().into()],
                block_number: 1,
                replacement_uuid: Some(uuid.to_string()),
                ..Default::default()
            }
            .try_into()
            .unwrap(),
            create_test_meter_bundle_response(),
        );

        assert_eq!(*bundle.uuid(), uuid);
        assert_eq!(bundle.replacement_uuid, Some(uuid));
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

    #[test]
    fn test_meter_bundle_response_serialization() {
        let response = MeterBundleResponse {
            bundle_gas_price: U256::from(1000000000),
            bundle_hash: B256::default(),
            coinbase_diff: U256::from(100),
            eth_sent_to_coinbase: U256::from(0),
            gas_fees: U256::from(100),
            results: vec![],
            state_block_number: 12345,
            state_flashblock_index: Some(42),
            total_gas_used: 21000,
            total_execution_time_us: 1000,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"stateFlashblockIndex\":42"));
        assert!(json.contains("\"stateBlockNumber\":12345"));

        let deserialized: MeterBundleResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.state_flashblock_index, Some(42));
        assert_eq!(deserialized.state_block_number, 12345);
    }

    #[test]
    fn test_meter_bundle_response_without_flashblock_index() {
        let response = MeterBundleResponse {
            bundle_gas_price: U256::from(1000000000),
            bundle_hash: B256::default(),
            coinbase_diff: U256::from(100),
            eth_sent_to_coinbase: U256::from(0),
            gas_fees: U256::from(100),
            results: vec![],
            state_block_number: 12345,
            state_flashblock_index: None,
            total_gas_used: 21000,
            total_execution_time_us: 1000,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("stateFlashblockIndex"));
        assert!(json.contains("\"stateBlockNumber\":12345"));

        let deserialized: MeterBundleResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.state_flashblock_index, None);
        assert_eq!(deserialized.state_block_number, 12345);
    }

    #[test]
    fn test_meter_bundle_response_deserialization_without_flashblock() {
        let json = r#"{
            "bundleGasPrice": "1000000000",
            "bundleHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "coinbaseDiff": "100",
            "ethSentToCoinbase": "0",
            "gasFees": "100",
            "results": [],
            "stateBlockNumber": 12345,
            "totalGasUsed": 21000,
            "totalExecutionTimeUs": 1000,
            "stateRootTimeUs": 500
        }"#;

        let deserialized: MeterBundleResponse = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized.bundle_gas_price, U256::from(1000000000));
        assert_eq!(deserialized.coinbase_diff, U256::from(100));
        assert_eq!(deserialized.eth_sent_to_coinbase, U256::from(0));
        assert_eq!(deserialized.state_flashblock_index, None);
        assert_eq!(deserialized.state_block_number, 12345);
        assert_eq!(deserialized.total_gas_used, 21000);
    }
}
