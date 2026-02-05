//! JSON-serializable types for dashboard data.
//!
//! These types match the Nethermind monitoring dashboard format for compatibility.

use serde::{Deserialize, Serialize};

/// Node information sent on initial connection and periodically.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeData {
    /// Process uptime in seconds.
    pub uptime: u64,
    /// Instance identifier.
    pub instance: String,
    /// Network name (e.g., "base", "base-sepolia").
    pub network: String,
    /// Sync type (e.g., "Full", "Snap").
    pub sync_type: String,
    /// Pruning mode.
    pub pruning_mode: String,
    /// Client version.
    pub version: String,
    /// Git commit hash.
    pub commit: String,
    /// Runtime info (e.g., "Rust").
    pub runtime: String,
    /// Native gas token symbol (e.g., "ETH").
    pub gas_token: String,
}

/// System resource statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SystemData {
    /// Process uptime in seconds.
    pub uptime: u64,
    /// User CPU percentage (0.0-1.0).
    pub user_percent: f64,
    /// Privileged/kernel CPU percentage (0.0-1.0).
    pub privileged_percent: f64,
    /// Working set memory in bytes.
    pub working_set: u64,
}

/// P2P peer information for pie chart.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerData {
    /// Number of connection contexts.
    pub contexts: u32,
    /// Client type ID (0=Unknown, 1=Nethermind, 2=Geth, etc.).
    pub client_type: u8,
    /// Client protocol version.
    pub version: u32,
    /// Peer's head block number.
    pub head: u64,
}

/// Fork choice update with full block data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkChoiceData {
    /// The head block with full transaction data.
    pub head: BlockForWeb,
    /// Safe block hash (hex string).
    pub safe: String,
    /// Finalized block hash (hex string).
    pub finalized: String,
}

/// Block data for web display (all values as hex strings for JS compatibility).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockForWeb {
    /// Extra data (hex string).
    pub extra_data: String,
    /// Gas limit (hex string).
    pub gas_limit: String,
    /// Gas used (hex string).
    pub gas_used: String,
    /// Block hash (hex string).
    pub hash: String,
    /// Block beneficiary/coinbase (hex string).
    pub beneficiary: String,
    /// Block number (hex string).
    pub number: String,
    /// Block size in bytes (hex string).
    pub size: String,
    /// Block timestamp (hex string).
    pub timestamp: String,
    /// Base fee per gas (hex string).
    pub base_fee_per_gas: String,
    /// Blob gas used (hex string).
    pub blob_gas_used: String,
    /// Excess blob gas (hex string).
    pub excess_blob_gas: String,
    /// Transactions in the block.
    pub tx: Vec<TransactionForWeb>,
    /// Receipts matching transactions 1:1.
    pub receipts: Vec<ReceiptForWeb>,
}

/// Transaction data for web display (all values as hex strings).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionForWeb {
    /// Transaction hash (hex string).
    pub hash: String,
    /// Sender address (hex string).
    pub from: String,
    /// Recipient address (hex string, empty for contract creation).
    pub to: String,
    /// Transaction type (0=legacy, 1=2930, 2=1559, 3=blob).
    pub tx_type: u8,
    /// Max priority fee per gas (hex string).
    pub max_priority_fee_per_gas: String,
    /// Max fee per gas (hex string).
    pub max_fee_per_gas: String,
    /// Gas price (hex string).
    pub gas_price: String,
    /// Gas limit (hex string).
    pub gas_limit: String,
    /// Transaction nonce (hex string).
    pub nonce: String,
    /// Value transferred (hex string).
    pub value: String,
    /// Input data length in bytes.
    pub data_length: usize,
    /// Number of blobs (for type 3 transactions).
    pub blobs: u8,
    /// Method selector (first 4 bytes of input, hex string).
    pub method: String,
}

/// Receipt data for web display.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptForWeb {
    /// Gas used by this transaction (hex string).
    pub gas_used: String,
    /// Effective gas price (hex string).
    pub effective_gas_price: String,
    /// Contract address if deployment (hex string).
    pub contract_address: String,
    /// Blob gas price (hex string).
    pub blob_gas_price: String,
    /// Blob gas used (hex string).
    pub blob_gas_used: String,
    /// Transaction logs.
    pub logs: Vec<LogForWeb>,
    /// Status (hex string, "0x1" for success).
    pub status: String,
}

/// Log entry for web display.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogForWeb {
    /// Contract address (hex string).
    pub address: String,
    /// Log data (hex string).
    pub data: String,
    /// Log topics (hex strings).
    pub topics: Vec<String>,
}

/// Transaction pool Sankey node definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxNode {
    /// Node name/id.
    pub name: String,
    /// Whether this is an inclusion node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inclusion: Option<bool>,
}

/// Transaction pool Sankey link (flow between nodes).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxLink {
    /// Source node name.
    pub source: String,
    /// Target node name.
    pub target: String,
    /// Flow value.
    pub value: u64,
}

/// Transaction pool data with Sankey links.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TxPoolData {
    /// Number of execution transactions in pool.
    pub pooled_tx: usize,
    /// Number of blob transactions in pool.
    pub pooled_blob_tx: usize,
    /// Number of transaction hashes received via P2P.
    pub hashes_received: u64,
    /// Sankey diagram links.
    pub links: Vec<TxLink>,
}

/// Block processing statistics for gas info display.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProcessedData {
    /// Number of blocks processed.
    pub block_count: u64,
    /// Starting block number.
    pub block_from: u64,
    /// Ending block number.
    pub block_to: u64,
    /// Processing time in milliseconds.
    pub processing_ms: u64,
    /// Slot time in milliseconds.
    pub slot_ms: u64,
    /// Megagas per second.
    pub mgas_per_second: f64,
    /// Minimum gas price in gwei.
    pub min_gas: f64,
    /// Median gas price in gwei.
    pub median_gas: f64,
    /// Average gas price in gwei.
    pub ave_gas: f64,
    /// Maximum gas price in gwei.
    pub max_gas: f64,
    /// Gas limit.
    pub gas_limit: u64,
}

/// Wrapper for SSE events with type discrimination.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DashboardEvent {
    /// Initial node information.
    NodeData(NodeData),
    /// Fork choice update with block data.
    ForkChoice(ForkChoiceData),
    /// System resource stats.
    System(SystemData),
    /// Peer statistics (array of peers).
    Peers(Vec<PeerData>),
    /// Transaction pool Sankey nodes (sent once on connect).
    TxNodes(Vec<TxNode>),
    /// Transaction pool Sankey links (sent periodically).
    TxLinks(TxPoolData),
    /// Block processing stats.
    Processed(ProcessedData),
    /// Log line for console display.
    Log(String),
}

impl DashboardEvent {
    /// Returns the event type name for SSE.
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::NodeData(_) => "nodeData",
            Self::ForkChoice(_) => "forkChoice",
            Self::System(_) => "system",
            Self::Peers(_) => "peers",
            Self::TxNodes(_) => "txNodes",
            Self::TxLinks(_) => "txLinks",
            Self::Processed(_) => "processed",
            Self::Log(_) => "log",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_data_serializes_to_camel_case() {
        let data = SystemData {
            uptime: 100,
            user_percent: 0.5,
            privileged_percent: 0.1,
            working_set: 1024,
        };
        let json = serde_json::to_string(&data).unwrap();

        // Verify camelCase keys (not snake_case)
        assert!(json.contains("\"userPercent\""), "expected userPercent, got: {json}");
        assert!(json.contains("\"workingSet\""), "expected workingSet, got: {json}");
        assert!(json.contains("\"privilegedPercent\""), "expected privilegedPercent, got: {json}");
        assert!(!json.contains("user_percent"), "should not contain snake_case");
        assert!(!json.contains("working_set"), "should not contain snake_case");
    }

    #[test]
    fn test_block_for_web_hex_format() {
        let block = BlockForWeb {
            extra_data: "0xabcd".to_string(),
            gas_limit: "0x1000".to_string(),
            gas_used: "0x800".to_string(),
            hash: "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
                .to_string(),
            beneficiary: "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            number: "0x10".to_string(),
            size: "0x200".to_string(),
            timestamp: "0x60000000".to_string(),
            base_fee_per_gas: "0x3b9aca00".to_string(),
            blob_gas_used: "0x0".to_string(),
            excess_blob_gas: "0x0".to_string(),
            tx: vec![],
            receipts: vec![],
        };

        let json = serde_json::to_string(&block).unwrap();

        // Verify all numeric fields are 0x-prefixed hex strings
        assert!(json.contains("\"gasLimit\":\"0x"), "gasLimit should be 0x-prefixed hex");
        assert!(json.contains("\"gasUsed\":\"0x"), "gasUsed should be 0x-prefixed hex");
        assert!(json.contains("\"hash\":\"0x"), "hash should be 0x-prefixed hex");
        assert!(json.contains("\"number\":\"0x"), "number should be 0x-prefixed hex");
        assert!(
            json.contains("\"baseFeePerGas\":\"0x"),
            "baseFeePerGas should be 0x-prefixed hex"
        );
    }

    #[test]
    fn test_dashboard_event_types() {
        let events = [
            (
                DashboardEvent::NodeData(NodeData {
                    uptime: 0,
                    instance: String::new(),
                    network: String::new(),
                    sync_type: String::new(),
                    pruning_mode: String::new(),
                    version: String::new(),
                    commit: String::new(),
                    runtime: String::new(),
                    gas_token: String::new(),
                }),
                "nodeData",
            ),
            (
                DashboardEvent::ForkChoice(ForkChoiceData {
                    head: BlockForWeb {
                        extra_data: String::new(),
                        gas_limit: String::new(),
                        gas_used: String::new(),
                        hash: String::new(),
                        beneficiary: String::new(),
                        number: String::new(),
                        size: String::new(),
                        timestamp: String::new(),
                        base_fee_per_gas: String::new(),
                        blob_gas_used: String::new(),
                        excess_blob_gas: String::new(),
                        tx: vec![],
                        receipts: vec![],
                    },
                    safe: String::new(),
                    finalized: String::new(),
                }),
                "forkChoice",
            ),
            (DashboardEvent::System(SystemData::default()), "system"),
            (DashboardEvent::Peers(vec![]), "peers"),
            (DashboardEvent::TxNodes(vec![]), "txNodes"),
            (DashboardEvent::TxLinks(TxPoolData::default()), "txLinks"),
            (DashboardEvent::Processed(ProcessedData::default()), "processed"),
            (DashboardEvent::Log(String::new()), "log"),
        ];

        for (event, expected_type) in events {
            assert_eq!(
                event.event_type(),
                expected_type,
                "wrong event type for {:?}",
                std::mem::discriminant(&event)
            );
        }
    }
}
