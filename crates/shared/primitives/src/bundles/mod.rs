use alloy_consensus::transaction::{Recovered, SignerRecoverable};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, TxHash, U256};
use base_alloy_consensus::OpTxEnvelope;
use base_bundles::{BundleExtensions, BundleTxs};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `Bundle` is the type that mirrors `EthSendBundle` and is used for the API.
#[derive(Default, Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Bundle {
    /// Raw RLP-encoded transaction bytes included in this bundle.
    pub txs: Vec<Bytes>,

    /// Target L2 block number for bundle inclusion.
    #[serde(with = "alloy_serde::quantity")]
    pub block_number: u64,

    /// Minimum flashblock index within the target block for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub flashblock_number_min: Option<u64>,

    /// Maximum flashblock index within the target block for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub flashblock_number_max: Option<u64>,

    /// Earliest block timestamp at which this bundle is valid.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub min_timestamp: Option<u64>,

    /// Latest block timestamp at which this bundle is valid.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_timestamp: Option<u64>,

    /// Transaction hashes that are allowed to revert without invalidating the bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverting_tx_hashes: Vec<TxHash>,

    /// UUID used to identify and replace a previously submitted bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_uuid: Option<String>,

    /// Transaction hashes whose pending bundles should be dropped when this bundle is accepted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropping_tx_hashes: Vec<TxHash>,
}

/// `ParsedBundle` is the type that contains utility methods for the `Bundle` type.
///
/// Unlike [`Bundle`], transactions are decoded and signer-recovered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedBundle {
    /// Decoded and signer-recovered transaction envelopes.
    pub txs: Vec<Recovered<OpTxEnvelope>>,
    /// Target L2 block number for bundle inclusion.
    pub block_number: u64,
    /// Minimum flashblock index within the target block for inclusion.
    pub flashblock_number_min: Option<u64>,
    /// Maximum flashblock index within the target block for inclusion.
    pub flashblock_number_max: Option<u64>,
    /// Earliest block timestamp at which this bundle is valid.
    pub min_timestamp: Option<u64>,
    /// Latest block timestamp at which this bundle is valid.
    pub max_timestamp: Option<u64>,
    /// Transaction hashes that are allowed to revert without invalidating the bundle.
    pub reverting_tx_hashes: Vec<TxHash>,
    /// Parsed UUID used to identify and replace a previously submitted bundle.
    pub replacement_uuid: Option<Uuid>,
    /// Transaction hashes whose pending bundles should be dropped when this bundle is accepted.
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

/// Response payload containing the computed hash of a submitted bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleHash {
    /// Keccak-256 hash over the concatenated transaction hashes in the bundle.
    pub bundle_hash: B256,
}

/// Request payload for cancelling a previously submitted bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelBundle {
    /// UUID identifying the bundle to cancel.
    pub replacement_uuid: String,
}

/// `AcceptedBundle` is the type that is sent over the wire.
///
/// Wraps a [`ParsedBundle`] with an assigned UUID and simulation results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedBundle {
    /// Unique identifier for this bundle, derived from `replacement_uuid` or the bundle hash.
    pub uuid: Uuid,

    /// Decoded and signer-recovered transaction envelopes.
    pub txs: Vec<Recovered<OpTxEnvelope>>,

    /// Target L2 block number for bundle inclusion.
    #[serde(with = "alloy_serde::quantity")]
    pub block_number: u64,

    /// Minimum flashblock index within the target block for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub flashblock_number_min: Option<u64>,

    /// Maximum flashblock index within the target block for inclusion.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub flashblock_number_max: Option<u64>,

    /// Earliest block timestamp at which this bundle is valid.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub min_timestamp: Option<u64>,

    /// Latest block timestamp at which this bundle is valid.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_timestamp: Option<u64>,

    /// Transaction hashes that are allowed to revert without invalidating the bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverting_tx_hashes: Vec<TxHash>,

    /// Parsed UUID used to identify and replace a previously submitted bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_uuid: Option<Uuid>,

    /// Transaction hashes whose pending bundles should be dropped when this bundle is accepted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropping_tx_hashes: Vec<TxHash>,

    /// Simulation results for the bundle from the meter service.
    pub meter_bundle_response: MeterBundleResponse,
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
    /// Creates a new [`AcceptedBundle`] from a parsed bundle and its simulation results.
    pub fn new(bundle: ParsedBundle, meter_bundle_response: MeterBundleResponse) -> Self {
        Self {
            uuid: bundle.replacement_uuid.unwrap_or_else(|| {
                Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice())
            }),
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

    /// Returns the UUID assigned to this bundle.
    pub const fn uuid(&self) -> &Uuid {
        &self.uuid
    }
}

/// Per-transaction simulation result from the meter service.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TransactionResult {
    /// Change in coinbase balance caused by this transaction.
    pub coinbase_diff: U256,
    /// ETH explicitly transferred to the coinbase address.
    pub eth_sent_to_coinbase: U256,
    /// Sender address of the transaction.
    pub from_address: Address,
    /// Total gas fees paid by this transaction.
    pub gas_fees: U256,
    /// Effective gas price of this transaction.
    pub gas_price: U256,
    /// Amount of gas consumed during execution.
    pub gas_used: u64,
    /// Recipient address, or `None` for contract creation.
    pub to_address: Option<Address>,
    /// Hash of the transaction.
    pub tx_hash: TxHash,
    /// ETH value transferred by this transaction.
    pub value: U256,
    /// Wall-clock execution time of this transaction in microseconds.
    pub execution_time_us: u128,
}

/// Aggregate simulation results for an entire bundle from the meter service.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MeterBundleResponse {
    /// Effective gas price for the bundle as a whole.
    pub bundle_gas_price: U256,
    /// Computed hash of the bundle.
    pub bundle_hash: B256,
    /// Total change in coinbase balance caused by all transactions in the bundle.
    pub coinbase_diff: U256,
    /// Total ETH explicitly transferred to the coinbase address across the bundle.
    pub eth_sent_to_coinbase: U256,
    /// Total gas fees paid across all transactions in the bundle.
    pub gas_fees: U256,
    /// Per-transaction simulation results.
    pub results: Vec<TransactionResult>,
    /// Block number of the state used for simulation.
    pub state_block_number: u64,
    /// Flashblock index of the state used for simulation, if applicable.
    #[serde(
        default,
        deserialize_with = "alloy_serde::quantity::opt::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub state_flashblock_index: Option<u64>,
    /// Total gas consumed by all transactions in the bundle.
    pub total_gas_used: u64,
    /// Total wall-clock execution time for the bundle in microseconds.
    pub total_execution_time_us: u128,
    /// Time spent computing the state root in microseconds.
    #[serde(default)]
    pub state_root_time_us: u128,
}

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Keccak256, keccak256};
    use alloy_signer_local::PrivateKeySigner;

    use super::*;
    use crate::test_utils::{create_test_meter_bundle_response, create_transaction};

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

        // Bundle hashes are keccak256(...txnHashes)
        let expected_bundle_hash_single = {
            let mut hasher = Keccak256::default();
            hasher.update(keccak256(&tx1_bytes));
            hasher.finalize()
        };

        assert_eq!(bundle.bundle_hash(), expected_bundle_hash_single);

        let uuid = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
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
            state_root_time_us: 0,
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
            state_root_time_us: 0,
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

    #[test]
    fn test_same_uuid_for_same_bundle_hash() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        // suppose this is a spam tx
        let tx1 = create_transaction(alice, 1, bob.address());
        let tx1_bytes = tx1.encoded_2718();

        // we receive it the first time
        let bundle1 = AcceptedBundle::new(
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

        // but we may receive it more than once
        let bundle2 = AcceptedBundle::new(
            Bundle {
                txs: vec![tx1_bytes.into()],
                block_number: 1,
                replacement_uuid: None,
                ..Default::default()
            }
            .try_into()
            .unwrap(),
            create_test_meter_bundle_response(),
        );

        // however, the UUID should be the same
        assert_eq!(bundle1.uuid(), bundle2.uuid());
    }
}
