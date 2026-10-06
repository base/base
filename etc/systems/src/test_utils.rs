//! Test fixtures shared by the crate's unit tests.

use std::collections::HashMap;

use alloy_consensus::{
    Block, BlockBody, Header, Sealed,
    transaction::{Recovered, TransactionInfo},
};
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::BlockTransactions;
use base_common_consensus::{BaseTransactionInfo, BaseTxEnvelope, TxDeposit};
use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig};
use base_common_rpc_types::{BaseBlockResponse, BaseHeaderResponse, Transaction};
use base_protocol::{L1BlockInfoBedrock, L1BlockInfoTx};
use jsonrpsee::{
    RpcModule,
    server::{ServerBuilder, ServerHandle},
    types::ErrorObjectOwned,
};
use serde_json::{Value, json};
use url::Url;

/// L2 blocks served over a minimal JSON-RPC node, keyed by their `eth_getBlockByNumber` tag.
///
/// Snapshot readers use a concrete HTTP provider, so tests serve real JSON-RPC responses instead
/// of mocking a trait.
#[derive(Debug)]
pub struct SnapshotRpcFixture {
    /// Genesis block pinned by [`Self::rollup_config`]; served as `0x0`.
    pub genesis: Block<BaseTxEnvelope>,
    /// RPC block responses keyed by tag, such as `latest` or `0x2`.
    pub blocks: HashMap<String, Value>,
}

impl Default for SnapshotRpcFixture {
    /// Builds a chain whose finalized, safe, and latest heads are blocks 1, 2, and 3, with L1
    /// origins 10, 10, and 11 and sequence numbers 0, 1, and 0.
    fn default() -> Self {
        let genesis = Block::<BaseTxEnvelope> {
            header: Header { number: 0, gas_limit: 30_000_000, ..Default::default() },
            body: BlockBody::default(),
        };
        let finalized = Self::l2_block(&genesis.header, 10, 0);
        let safe = Self::l2_block(&finalized.header, 10, 1);
        let latest = Self::l2_block(&safe.header, 11, 0);
        let blocks = HashMap::from([
            ("0x0".to_string(), Self::rpc_block(genesis.clone())),
            ("finalized".to_string(), Self::rpc_block(finalized)),
            ("safe".to_string(), Self::rpc_block(safe)),
            ("latest".to_string(), Self::rpc_block(latest)),
        ]);
        Self { genesis, blocks }
    }
}

impl SnapshotRpcFixture {
    /// L2 chain ID of the fixture chain.
    pub const CHAIN_ID: u64 = 8453;
    /// Batcher address encoded in every L1-info deposit.
    pub const BATCHER: Address = Address::repeat_byte(0xba);

    /// Returns a rollup configuration whose genesis matches the fixture chain.
    pub fn rollup_config(&self) -> RollupConfig {
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

    /// Builds the child of `parent` whose L1-info deposit references L1 block `l1_number`.
    ///
    /// The L1 origin hash repeats the low byte of `l1_number`.
    pub fn l2_block(parent: &Header, l1_number: u64, seq_num: u64) -> Block<BaseTxEnvelope> {
        let l1_info = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::new(
            l1_number,
            l1_number * 12,
            1,
            B256::repeat_byte(l1_number as u8),
            seq_num,
            Self::BATCHER,
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

    /// Encodes `block` as a full-transaction `eth_getBlockByNumber` response.
    pub fn rpc_block(block: Block<BaseTxEnvelope>) -> Value {
        let header = Sealed::new(block.header);
        let transactions = block
            .body
            .transactions
            .into_iter()
            .enumerate()
            .map(|(index, transaction)| {
                Transaction::from_transaction(
                    Recovered::new_unchecked(transaction, Address::ZERO),
                    BaseTransactionInfo {
                        inner: TransactionInfo {
                            index: Some(index as u64),
                            block_hash: Some(header.hash()),
                            block_number: Some(header.number),
                            base_fee: header.base_fee_per_gas,
                            block_timestamp: Some(header.timestamp),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
            })
            .collect();
        serde_json::to_value(BaseBlockResponse {
            header: BaseHeaderResponse::new(alloy_rpc_types_eth::Header::from_consensus(
                header, None, None,
            )),
            uncles: Vec::new(),
            transactions: BlockTransactions::Full(transactions),
            withdrawals: block.body.withdrawals,
        })
        .unwrap()
    }

    /// Serves `eth_chainId` as `chain_id` and the fixture blocks by tag or hash, returning `null`
    /// for any other block. Stop the returned handle to shut the server down.
    pub async fn serve(self, chain_id: u64) -> (Url, ServerHandle) {
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let mut module = RpcModule::new((chain_id, self.blocks));
        module
            .register_method("eth_chainId", |_, node, _| {
                Ok::<_, ErrorObjectOwned>(format!("{:#x}", node.0))
            })
            .unwrap();
        module
            .register_method("eth_getBlockByNumber", |params, node, _| {
                let (tag, _full): (String, bool) = params.parse()?;
                Ok::<_, ErrorObjectOwned>(node.1.get(&tag).cloned().unwrap_or(Value::Null))
            })
            .unwrap();
        module
            .register_method("eth_getBlockByHash", |params, node, _| {
                let (hash, _full): (B256, bool) = params.parse()?;
                let block = node.1.values().find(|block| block["hash"] == json!(hash));
                Ok::<_, ErrorObjectOwned>(block.cloned().unwrap_or(Value::Null))
            })
            .unwrap();
        (format!("http://{address}").parse().unwrap(), server.start(module))
    }
}
