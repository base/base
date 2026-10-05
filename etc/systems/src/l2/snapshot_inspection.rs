//! Read-only inspection of the labeled heads of a snapshot source execution node.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig};
use base_protocol::{BlockInfo, L2BlockInfo};
use eyre::{Result, ensure};
use serde::Serialize;
use url::Url;

use super::SnapshotBoundary;

/// Rollup metadata decoded from one labeled snapshot block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotLabeledBlock {
    /// L2 block identity, L1 origin, and sequence number from the block's L1-info deposit.
    pub block_info: L2BlockInfo,
    /// Effective system configuration recovered from the block.
    pub system_config: SystemConfig,
}

impl From<SnapshotBoundary> for SnapshotLabeledBlock {
    fn from(boundary: SnapshotBoundary) -> Self {
        Self { block_info: boundary.l2_block_info, system_config: boundary.system_config }
    }
}

/// Validated latest, safe, and finalized heads of a snapshot source node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotInspection {
    /// L2 chain ID reported by the node.
    pub chain_id: u64,
    /// Rollup genesis pinned by the selected chain configuration.
    ///
    /// It is trusted rather than re-read because snapshot nodes may prune the genesis body; the
    /// labeled heads still prove the node serves the expected chain ID.
    pub genesis: ChainGenesis,
    /// Resolved rollup configuration, unchanged.
    pub rollup_config: RollupConfig,
    /// Unsafe head.
    pub latest: SnapshotLabeledBlock,
    /// Safe head.
    pub safe: SnapshotLabeledBlock,
    /// Finalized head.
    pub finalized: SnapshotLabeledBlock,
    /// Canonical L1 block that completes derivation of the unsafe tail, when fork discovery ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork: Option<BlockInfo>,
}

impl SnapshotInspection {
    /// Reads all three labeled heads over `rpc_url` without touching any local datadir.
    ///
    /// Fails when the chain ID does not match, or when any label is missing or lacks a real
    /// L1-info deposit, such as a pruned body. The genesis block itself is never fetched.
    pub async fn read(
        rpc_url: Url,
        rollup_config: Arc<RollupConfig>,
        expected_chain_id: u64,
    ) -> Result<Self> {
        // Labels only advance, so reading the most stable label first keeps their order
        // consistent while the node keeps progressing.
        let finalized = SnapshotBoundary::read(
            rpc_url.clone(),
            Arc::clone(&rollup_config),
            expected_chain_id,
            BlockNumberOrTag::Finalized,
            None,
        )
        .await?;
        let safe = SnapshotBoundary::read(
            rpc_url.clone(),
            Arc::clone(&rollup_config),
            expected_chain_id,
            BlockNumberOrTag::Safe,
            None,
        )
        .await?;
        let latest = SnapshotBoundary::read(
            rpc_url,
            Arc::clone(&rollup_config),
            expected_chain_id,
            BlockNumberOrTag::Latest,
            None,
        )
        .await?;
        ensure!(
            finalized.head.number <= safe.head.number && safe.head.number <= latest.head.number,
            "snapshot labels are out of order: finalized {}, safe {}, latest {}",
            finalized.head.number,
            safe.head.number,
            latest.head.number
        );

        Ok(Self {
            chain_id: expected_chain_id,
            genesis: rollup_config.genesis,
            rollup_config: (*rollup_config).clone(),
            latest: latest.into(),
            safe: safe.into(),
            finalized: finalized.into(),
            fork: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use alloy_consensus::{Block, BlockBody, Header, Sealed, transaction::Recovered};
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Address, B256, U256};
    use alloy_rpc_types_eth::BlockTransactions;
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig};
    use base_common_rpc_types::{BaseBlockResponse, BaseHeaderResponse, Transaction};
    use base_protocol::{L1BlockInfoBedrock, L1BlockInfoTx};
    use jsonrpsee::{
        RpcModule,
        server::{ServerBuilder, ServerHandle},
        types::ErrorObjectOwned,
    };
    use serde_json::Value;
    use url::Url;

    use super::SnapshotInspection;

    const CHAIN_ID: u64 = 8453;
    const BATCHER: Address = Address::repeat_byte(0xba);

    #[derive(Clone)]
    struct MockNode {
        chain_id: u64,
        blocks: Arc<HashMap<String, Value>>,
    }

    struct Fixture {
        genesis: Block<BaseTxEnvelope>,
        blocks: HashMap<String, Value>,
    }

    impl Fixture {
        /// Builds a chain whose finalized, safe, and latest heads are blocks 1, 2, and 3.
        fn new() -> Self {
            let genesis = Block::<BaseTxEnvelope> {
                header: Header { number: 0, gas_limit: 30_000_000, ..Default::default() },
                body: BlockBody::default(),
            };
            let finalized = l2_block(&genesis.header, 10, 0);
            let safe = l2_block(&finalized.header, 10, 1);
            let latest = l2_block(&safe.header, 11, 0);
            let blocks = HashMap::from([
                ("0x0".to_string(), rpc_block(genesis.clone())),
                ("finalized".to_string(), rpc_block(finalized)),
                ("safe".to_string(), rpc_block(safe)),
                ("latest".to_string(), rpc_block(latest)),
            ]);
            Self { genesis, blocks }
        }

        fn rollup_config(&self) -> RollupConfig {
            RollupConfig {
                genesis: ChainGenesis {
                    l1: BlockNumHash { number: 9, hash: B256::repeat_byte(9) },
                    l2: BlockNumHash { number: 0, hash: self.genesis.header.hash_slow() },
                    l2_time: 0,
                    system_config: Some(SystemConfig::default()),
                },
                ..Default::default()
            }
        }

        async fn inspect(self, rollup_config: RollupConfig) -> eyre::Result<SnapshotInspection> {
            let (url, handle) = serve(CHAIN_ID, self.blocks).await;
            let result = SnapshotInspection::read(url, Arc::new(rollup_config), CHAIN_ID).await;
            handle.stop().unwrap();
            result
        }
    }

    fn l2_block(parent: &Header, l1_number: u64, seq_num: u64) -> Block<BaseTxEnvelope> {
        let l1_info = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::new(
            l1_number,
            l1_number * 12,
            1,
            B256::repeat_byte(l1_number as u8),
            seq_num,
            BATCHER,
            U256::from(188),
            U256::from(684_000),
        ));
        let deposit = TxDeposit { input: l1_info.encode_calldata(), ..Default::default() };
        Block {
            header: Header {
                number: parent.number + 1,
                parent_hash: parent.hash_slow(),
                timestamp: parent.timestamp + 2,
                gas_limit: 30_000_000,
                ..Default::default()
            },
            body: BlockBody {
                transactions: vec![BaseTxEnvelope::Deposit(Sealed::new(deposit))],
                ..Default::default()
            },
        }
    }

    fn rpc_block(block: Block<BaseTxEnvelope>) -> Value {
        let hash = block.header.hash_slow();
        let number = block.header.number;
        let timestamp = block.header.timestamp;
        let transactions = block
            .body
            .transactions
            .into_iter()
            .enumerate()
            .map(|(index, transaction)| Transaction {
                inner: alloy_rpc_types_eth::Transaction {
                    inner: Recovered::new_unchecked(transaction, Address::ZERO),
                    block_hash: Some(hash),
                    block_number: Some(number),
                    block_timestamp: Some(timestamp),
                    transaction_index: Some(index as u64),
                    effective_gas_price: Some(0),
                },
                block_timestamp_ms: None,
                deposit_nonce: None,
                deposit_receipt_version: None,
            })
            .collect();
        serde_json::to_value(BaseBlockResponse {
            header: BaseHeaderResponse::new(alloy_rpc_types_eth::Header {
                hash,
                inner: block.header,
                total_difficulty: None,
                size: None,
            }),
            uncles: Vec::new(),
            transactions: BlockTransactions::Full(transactions),
            withdrawals: None,
        })
        .unwrap()
    }

    async fn serve(chain_id: u64, blocks: HashMap<String, Value>) -> (Url, ServerHandle) {
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let mut module = RpcModule::new(MockNode { chain_id, blocks: Arc::new(blocks) });
        module
            .register_method("eth_chainId", |_, node, _| {
                Ok::<_, ErrorObjectOwned>(format!("{:#x}", node.chain_id))
            })
            .unwrap();
        module
            .register_method("eth_getBlockByNumber", |params, node, _| {
                let (tag, _full): (String, bool) = params.parse()?;
                Ok::<_, ErrorObjectOwned>(node.blocks.get(&tag).cloned().unwrap_or(Value::Null))
            })
            .unwrap();
        (format!("http://{address}").parse().unwrap(), server.start(module))
    }

    #[tokio::test]
    async fn reads_distinct_labels_as_json() {
        let fixture = Fixture::new();
        let rollup_config = fixture.rollup_config();
        let inspection = fixture.inspect(rollup_config.clone()).await.unwrap();

        assert_eq!(inspection.rollup_config, rollup_config);
        let json = serde_json::to_value(&inspection).unwrap();
        assert_eq!(json["chain_id"], CHAIN_ID);
        assert_eq!(json["genesis"], serde_json::to_value(rollup_config.genesis).unwrap());
        assert_eq!(json["rollup_config"], serde_json::to_value(&rollup_config).unwrap());
        assert!(json.get("fork").is_none(), "fork is reported only by fork discovery");
        for (label, number, l1_number, seq_num) in
            [("finalized", 1, 10, 0), ("safe", 2, 10, 1), ("latest", 3, 11, 0)]
        {
            let block_info = &json[label]["block_info"];
            assert_eq!(block_info["number"], number, "{label}");
            assert_eq!(block_info["timestamp"], number * 2, "{label}");
            assert_eq!(block_info["l1origin"]["number"], l1_number, "{label}");
            assert_eq!(
                block_info["l1origin"]["hash"],
                serde_json::to_value(B256::repeat_byte(l1_number as u8)).unwrap(),
                "{label}"
            );
            assert_eq!(block_info["sequenceNumber"], seq_num, "{label}");
            assert_eq!(
                json[label]["system_config"]["batcherAddr"],
                serde_json::to_value(BATCHER).unwrap(),
                "{label}"
            );
        }
        assert_eq!(json["safe"]["block_info"]["hash"], json["latest"]["block_info"]["parentHash"]);
        assert_ne!(inspection.safe, inspection.latest);
    }

    #[tokio::test]
    async fn reads_trusted_snapshot_without_genesis_body() {
        let mut fixture = Fixture::new();
        let rollup_config = fixture.rollup_config();
        fixture.blocks.remove("0x0");

        let inspection = fixture.inspect(rollup_config.clone()).await.unwrap();
        assert_eq!(inspection.genesis, rollup_config.genesis);
        assert_eq!(inspection.latest.block_info.block_info.number, 3);
        assert_eq!(inspection.safe.block_info.block_info.number, 2);
    }

    #[tokio::test]
    async fn rejects_missing_label() {
        let mut fixture = Fixture::new();
        let rollup_config = fixture.rollup_config();
        fixture.blocks.remove("safe");

        let error = fixture.inspect(rollup_config).await.unwrap_err();
        assert!(error.to_string().contains("has no safe block"), "{error:?}");
    }

    #[tokio::test]
    async fn rejects_pruned_label() {
        let mut fixture = Fixture::new();
        let rollup_config = fixture.rollup_config();
        let pruned = Block::<BaseTxEnvelope> {
            header: Header {
                number: 1,
                parent_hash: fixture.genesis.header.hash_slow(),
                ..Default::default()
            },
            body: BlockBody::default(),
        };
        fixture.blocks.insert("finalized".to_string(), rpc_block(pruned));

        let error = fixture.inspect(rollup_config).await.unwrap_err();
        assert!(format!("{error:#}").contains("failed to derive L2 block info"), "{error:?}");
    }

    #[tokio::test]
    async fn rejects_genesis_as_label() {
        let mut fixture = Fixture::new();
        let rollup_config = fixture.rollup_config();
        let genesis = fixture.blocks["0x0"].clone();
        fixture.blocks.insert("finalized".to_string(), genesis);

        let error = fixture.inspect(rollup_config).await.unwrap_err();
        assert!(error.to_string().contains("not an L1-info deposit"), "{error:?}");
    }

    #[tokio::test]
    async fn rejects_chain_id_mismatch() {
        let fixture = Fixture::new();
        let rollup_config = fixture.rollup_config();
        let (url, handle) = serve(CHAIN_ID + 1, fixture.blocks).await;

        let error =
            SnapshotInspection::read(url, Arc::new(rollup_config), CHAIN_ID).await.unwrap_err();
        handle.stop().unwrap();
        assert!(error.to_string().contains("does not match expected chain ID"), "{error:?}");
    }
}
