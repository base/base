use std::sync::Arc;

use alloy_consensus::Receipt;
use alloy_primitives::{b256, bytes, hex::FromHex, map::HashMap, Address, Bytes, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use base_reth_flashblocks_rpc::subscription::{Flashblock, Metadata};
use op_alloy_consensus::OpDepositReceipt;
use reth::api::{NodeTypes, NodeTypesWithDBAdapter};
use reth_db::{
    init_db,
    mdbx::{DatabaseArguments, MaxReadTransactionDuration, KILOBYTE, MEGABYTE},
    test_utils::{create_test_static_files_dir, tempdir_path, TempDatabase, ERROR_DB_CREATION},
    ClientVersion, DatabaseEnv,
};
use reth_optimism_primitives::OpReceipt;
use reth_provider::{providers::StaticFileProvider, ProviderFactory};
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};

const FLASHBLOCK_PAYLOAD_ID: [u8; 8] = [0; 8];
// Pre-captured deposit transaction and hash used by flashblocks tests for the L1 block info deposit.
// Values match `BLOCK_INFO_TXN` and `BLOCK_INFO_TXN_HASH` from `crates/flashblocks-rpc/src/tests/mod.rs`.
pub const BLOCK_INFO_DEPOSIT_TX: Bytes = bytes!("0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000");
pub const BLOCK_INFO_DEPOSIT_TX_HASH: B256 =
    b256!("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6");

pub fn secret_from_hex(hex_key: &str) -> B256 {
    B256::from_hex(hex_key).expect("32-byte private key")
}

pub fn create_provider_factory<N: NodeTypes>(
    chain_spec: Arc<N::ChainSpec>,
) -> ProviderFactory<NodeTypesWithDBAdapter<N, Arc<TempDatabase<DatabaseEnv>>>> {
    let (static_dir, _) = create_test_static_files_dir();
    let db = create_test_db();
    ProviderFactory::new(
        db,
        chain_spec,
        StaticFileProvider::read_write(static_dir.keep()).expect("static file provider"),
    )
}

fn create_test_db() -> Arc<TempDatabase<DatabaseEnv>> {
    let path = tempdir_path();
    let emsg = format!("{ERROR_DB_CREATION}: {path:?}");

    let db = init_db(
        &path,
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            .with_geometry_max_size(Some(4 * MEGABYTE))
            .with_growth_step(Some(4 * KILOBYTE)),
    )
    .expect(&emsg);

    Arc::new(TempDatabase::new(db, path))
}

pub fn build_single_flashblock(
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
            inner: Receipt {
                status: true.into(),
                cumulative_gas_used: 10_000,
                logs: vec![],
            },
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
        metadata: Metadata {
            receipts,
            new_account_balances: Default::default(),
            block_number,
        },
    }
}
