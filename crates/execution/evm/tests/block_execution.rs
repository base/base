//! Integration tests for Base block execution behavior.

use std::{collections::HashMap, str::FromStr, sync::Arc};

use alloy_consensus::{Block, BlockBody, Header, SignableTransaction, TxEip1559};
use alloy_primitives::{Address, Signature, StorageKey, StorageValue, U256, address, b256, bytes};
use base_common_chains::BaseUpgrade;
use base_common_consensus::{
    BaseReceipt, BaseTransactionSigned, Predeploys, SystemAddresses, TxDeposit,
};
use base_common_evm::BaseTime;
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::{BaseEvmConfig, BaseRethReceiptBuilder};
use base_protocol::BaseTimeUpdateTx;
use reth_chainspec::{ForkCondition, MIN_TRANSACTION_GAS};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_primitives_traits::{Account, RecoveredBlock};
use reth_revm::{database::StateProviderDatabase, test_utils::StateProviderTest};

const BASE_TIME_READER: Address = address!("0x1000000000000000000000000000000000000000");
const USER: Address = address!("0x1000000000000000000000000000000000000001");
const BLOCK_TIMESTAMP: u64 = 1_725;
const PREVIOUS_MILLIS_PART: u16 = 200;
const CURRENT_MILLIS_PART: u16 = 600;

fn create_base_state_provider() -> StateProviderTest {
    let mut db = StateProviderTest::default();

    let l1_block_contract_account = Account { balance: U256::ZERO, bytecode_hash: None, nonce: 1 };

    let mut l1_block_storage = HashMap::default();
    // base fee
    l1_block_storage.insert(StorageKey::with_last_byte(1), StorageValue::from(1000000000));
    // l1 fee overhead
    l1_block_storage.insert(StorageKey::with_last_byte(5), StorageValue::from(188));
    // l1 fee scalar
    l1_block_storage.insert(StorageKey::with_last_byte(6), StorageValue::from(684000));
    // l1 free scalars post ecotone
    l1_block_storage.insert(
        StorageKey::with_last_byte(3),
        StorageValue::from_str(
            "0x0000000000000000000000000000000000001db0000d27300000000000000005",
        )
        .unwrap(),
    );

    db.insert_account(Predeploys::L1_BLOCK_INFO, l1_block_contract_account, None, l1_block_storage);

    db
}

fn evm_config(chain_spec: Arc<BaseChainSpec>) -> BaseEvmConfig {
    BaseEvmConfig::new(chain_spec, BaseRethReceiptBuilder::default())
}

fn execute_same_block_base_time_read(getter_selector: [u8; 4]) -> U256 {
    let mut db = create_base_state_provider();
    let mut base_time_storage = HashMap::default();
    base_time_storage.insert(
        StorageKey::from(BaseTime::ADMIN_SLOT.to_be_bytes::<32>()),
        U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()),
    );
    base_time_storage.insert(
        StorageKey::from(BaseTime::TIMESTAMP_MILLIS_PART_SLOT.to_be_bytes::<32>()),
        U256::from(PREVIOUS_MILLIS_PART),
    );
    db.insert_account(
        Predeploys::BASE_TIME,
        Account::default(),
        Some(BaseTime::proxy_bytecode()),
        base_time_storage,
    );
    db.insert_account(
        BASE_TIME_READER,
        Account::default(),
        // Forward the four-byte calldata to BaseTime and store its returned word in slot zero.
        Some(bytes!(
            "6004600060003760206000600460007342000000000000000000000000000000000000305afa5060005160005500"
        )),
        HashMap::default(),
    );
    db.insert_account(
        USER,
        Account { balance: U256::MAX, ..Default::default() },
        None,
        HashMap::default(),
    );

    let chain_spec = Arc::new(
        BaseChainSpecBuilder::base_mainnet()
            .cobalt_activated()
            .with_fork(BaseUpgrade::Zombie, ForkCondition::Timestamp(0))
            .build(),
    );

    let l1_info_tx: BaseTransactionSigned = TxDeposit {
        from: SystemAddresses::DEPOSITOR_ACCOUNT,
        to: Predeploys::L1_BLOCK_INFO.into(),
        gas_limit: 1_000_000,
        ..Default::default()
    }
    .into();
    let base_time_tx = BaseTimeUpdateTx::new(CURRENT_MILLIS_PART).unwrap().into_deposit_tx(1);
    let base_time_tx: BaseTransactionSigned = base_time_tx.into();
    let user_tx: BaseTransactionSigned = TxEip1559 {
        chain_id: chain_spec.chain.id(),
        nonce: 0,
        gas_limit: 100_000,
        to: BASE_TIME_READER.into(),
        input: getter_selector.into(),
        ..Default::default()
    }
    .into_signed(Signature::test_signature())
    .into();

    let header = Header {
        timestamp: BLOCK_TIMESTAMP,
        number: 1,
        gas_limit: 3_000_000,
        parent_beacon_block_root: Some(Default::default()),
        ..Default::default()
    };
    let output = BasicBlockExecutor::new(evm_config(chain_spec), StateProviderDatabase::new(&db))
        .execute(&RecoveredBlock::new_unhashed(
            Block {
                header,
                body: BlockBody {
                    transactions: vec![l1_info_tx, base_time_tx, user_tx],
                    ..Default::default()
                },
            },
            vec![SystemAddresses::DEPOSITOR_ACCOUNT, SystemAddresses::DEPOSITOR_ACCOUNT, USER],
        ))
        .expect("BaseTime metadata and user reads should execute in order");

    assert_eq!(output.receipts.len(), 3);
    assert_eq!(
        output.storage(&Predeploys::BASE_TIME, BaseTime::TIMESTAMP_MILLIS_PART_SLOT),
        Some(U256::from(CURRENT_MILLIS_PART))
    );
    output.storage(&BASE_TIME_READER, U256::ZERO).expect("reader should store the getter result")
}

#[test]
fn base_time_millis_part_is_visible_to_user_transaction_in_same_block() {
    assert_eq!(
        execute_same_block_base_time_read(BaseTime::TIMESTAMP_MILLIS_PART_SELECTOR),
        U256::from(CURRENT_MILLIS_PART)
    );
}

#[test]
fn base_time_timestamp_ms_is_visible_to_user_transaction_in_same_block() {
    assert_eq!(
        execute_same_block_base_time_read(BaseTime::TIMESTAMP_MS_SELECTOR),
        U256::from(BLOCK_TIMESTAMP * 1_000 + u64::from(CURRENT_MILLIS_PART))
    );
}

#[test]
fn base_deposit_fields_pre_canyon() {
    let header = Header {
        timestamp: 1,
        number: 1,
        gas_limit: 1_000_000,
        gas_used: 42_000,
        receipts_root: b256!("0x83465d1e7d01578c0d609be33570f91242f013e9e295b0879905346abbd63731"),
        ..Default::default()
    };

    let mut db = create_base_state_provider();

    let addr = Address::ZERO;
    let account = Account { balance: U256::MAX, ..Account::default() };
    db.insert_account(addr, account, None, HashMap::default());

    let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().regolith_activated().build());

    let tx: BaseTransactionSigned = TxEip1559 {
        chain_id: chain_spec.chain.id(),
        nonce: 0,
        gas_limit: MIN_TRANSACTION_GAS,
        to: addr.into(),
        ..Default::default()
    }
    .into_signed(Signature::test_signature())
    .into();

    let tx_deposit: BaseTransactionSigned = TxDeposit {
        from: addr,
        to: addr.into(),
        gas_limit: MIN_TRANSACTION_GAS,
        ..Default::default()
    }
    .into();

    let provider = evm_config(chain_spec);
    let mut executor = BasicBlockExecutor::new(provider, StateProviderDatabase::new(&db));

    executor.with_state_mut(|state| {
        state.load_cache_account(Predeploys::L1_BLOCK_INFO).unwrap();
    });

    let output = executor
        .execute(&RecoveredBlock::new_unhashed(
            Block {
                header,
                body: BlockBody { transactions: vec![tx, tx_deposit], ..Default::default() },
            },
            vec![addr, addr],
        ))
        .unwrap();

    let receipts = &output.receipts;
    let tx_receipt = &receipts[0];
    let deposit_receipt = &receipts[1];

    assert!(!matches!(tx_receipt, BaseReceipt::Deposit(_)));
    let BaseReceipt::Deposit(deposit_receipt) = deposit_receipt else { panic!("expected deposit") };
    assert!(deposit_receipt.deposit_nonce.is_some());
    assert!(deposit_receipt.deposit_receipt_version.is_none());
}

#[test]
fn base_deposit_fields_post_canyon() {
    let header = Header {
        timestamp: 2,
        number: 1,
        gas_limit: 1_000_000,
        gas_used: 42_000,
        receipts_root: b256!("0xfffc85c4004fd03c7bfbe5491fae98a7473126c099ac11e8286fd0013f15f908"),
        ..Default::default()
    };

    let mut db = create_base_state_provider();
    let addr = Address::ZERO;
    let account = Account { balance: U256::MAX, ..Account::default() };

    db.insert_account(addr, account, None, HashMap::default());

    let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().canyon_activated().build());

    let tx: BaseTransactionSigned = TxEip1559 {
        chain_id: chain_spec.chain.id(),
        nonce: 0,
        gas_limit: MIN_TRANSACTION_GAS,
        to: addr.into(),
        ..Default::default()
    }
    .into_signed(Signature::test_signature())
    .into();

    let tx_deposit: BaseTransactionSigned = TxDeposit {
        from: addr,
        to: addr.into(),
        gas_limit: MIN_TRANSACTION_GAS,
        ..Default::default()
    }
    .into();

    let provider = evm_config(chain_spec);
    let mut executor = BasicBlockExecutor::new(provider, StateProviderDatabase::new(&db));

    executor.with_state_mut(|state| {
        state.load_cache_account(Predeploys::L1_BLOCK_INFO).unwrap();
    });

    let output = executor
        .execute(&RecoveredBlock::new_unhashed(
            Block {
                header,
                body: BlockBody { transactions: vec![tx, tx_deposit], ..Default::default() },
            },
            vec![addr, addr],
        ))
        .expect("Executing a block while canyon is active should not fail");

    let receipts = &output.receipts;
    let tx_receipt = &receipts[0];
    let deposit_receipt = &receipts[1];

    assert!(!matches!(tx_receipt, BaseReceipt::Deposit(_)));
    let BaseReceipt::Deposit(deposit_receipt) = deposit_receipt else { panic!("expected deposit") };
    assert_eq!(deposit_receipt.deposit_receipt_version, Some(1));
    assert!(deposit_receipt.deposit_nonce.is_some());
}
