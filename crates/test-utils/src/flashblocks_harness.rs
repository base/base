use std::{collections::HashMap, ops::Deref, sync::Arc};

use alloy_consensus::Receipt;
use alloy_primitives::{Address, B256, Bytes, U256, b256, bytes};
use alloy_rpc_types_engine::PayloadId;
use base_reth_flashblocks_rpc::subscription::{Flashblock, Metadata};
use eyre::Result;
use futures_util::Future;
use op_alloy_consensus::OpDepositReceipt;
use reth::builder::NodeHandle;
use reth_e2e_test_utils::Adapter;
use reth_optimism_node::OpNode;
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};

use crate::{
    harness::TestHarness,
    node::{
        FlashblocksLocalNode, FlashblocksParts, LocalFlashblocksState, OpAddOns, OpBuilder,
        default_launcher,
    },
    tracing::init_silenced_tracing,
};

const FLASHBLOCK_PAYLOAD_ID: [u8; 8] = [0; 8];
const BLOCK_INFO_DEPOSIT_TX: Bytes = bytes!(
    "0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000"
);
const BLOCK_INFO_DEPOSIT_TX_HASH: B256 =
    b256!("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6");

pub struct FlashblocksHarness {
    inner: TestHarness,
    parts: FlashblocksParts,
}

impl FlashblocksHarness {
    pub async fn new() -> Result<Self> {
        Self::with_launcher(default_launcher).await
    }

    pub async fn manual_canonical() -> Result<Self> {
        Self::manual_canonical_with_launcher(default_launcher).await
    }

    pub async fn with_launcher<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        init_silenced_tracing();
        let flash_node = FlashblocksLocalNode::with_launcher(launcher).await?;
        Self::from_flashblocks_node(flash_node).await
    }

    pub async fn manual_canonical_with_launcher<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        init_silenced_tracing();
        let flash_node = FlashblocksLocalNode::with_manual_canonical_launcher(launcher).await?;
        Self::from_flashblocks_node(flash_node).await
    }

    pub fn flashblocks_state(&self) -> Arc<LocalFlashblocksState> {
        self.parts.state()
    }

    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.parts.send(flashblock).await
    }

    pub async fn send_flashblocks<I>(&self, flashblocks: I) -> Result<()>
    where
        I: IntoIterator<Item = Flashblock>,
    {
        for flashblock in flashblocks {
            self.send_flashblock(flashblock).await?;
        }
        Ok(())
    }

    /// Builds a flashblock payload for testing. Callers can intentionally pass invalid
    /// values (for example a zeroed beacon root) to assert how downstream components
    /// react to malformed flashblocks.
    #[allow(clippy::too_many_arguments)]
    pub fn build_flashblock(
        &self,
        block_number: u64,
        parent_hash: B256,
        parent_beacon_block_root: B256,
        timestamp: u64,
        gas_limit: u64,
        transactions: Vec<(Bytes, Option<(B256, OpReceipt)>)>,
    ) -> Flashblock {
        let base = ExecutionPayloadBaseV1 {
            parent_beacon_block_root,
            parent_hash,
            fee_recipient: Address::ZERO,
            prev_randao: B256::ZERO,
            block_number,
            gas_limit,
            timestamp,
            extra_data: Bytes::new(),
            base_fee_per_gas: U256::from(1),
        };

        let mut flashblock_txs = vec![BLOCK_INFO_DEPOSIT_TX.clone()];
        let mut receipts = HashMap::default();
        receipts.insert(
            BLOCK_INFO_DEPOSIT_TX_HASH,
            OpReceipt::Deposit(OpDepositReceipt {
                inner: Receipt { status: true.into(), cumulative_gas_used: 10_000, logs: vec![] },
                deposit_nonce: Some(4_012_991u64),
                deposit_receipt_version: None,
            }),
        );

        for (tx_bytes, maybe_receipt) in transactions {
            if let Some((hash, receipt)) = maybe_receipt {
                receipts.insert(hash, receipt);
            }
            flashblock_txs.push(tx_bytes);
        }

        Flashblock {
            payload_id: PayloadId::new(FLASHBLOCK_PAYLOAD_ID),
            index: 0,
            base: Some(base),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                transactions: flashblock_txs,
                ..Default::default()
            },
            metadata: Metadata { receipts, new_account_balances: Default::default(), block_number },
        }
    }

    pub fn into_inner(self) -> TestHarness {
        self.inner
    }

    async fn from_flashblocks_node(flash_node: FlashblocksLocalNode) -> Result<Self> {
        let (node, parts) = flash_node.into_parts();
        let inner = TestHarness::from_node(node).await?;
        Ok(Self { inner, parts })
    }
}

impl Deref for FlashblocksHarness {
    type Target = TestHarness;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
