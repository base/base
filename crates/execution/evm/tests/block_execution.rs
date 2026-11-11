//! Integration tests for Base block execution behavior.

use std::{
    str::FromStr,
    sync::{Arc, Mutex},
};

use alloy_consensus::{
    Block, BlockBody, Header, SignableTransaction, TxEip1559, transaction::Recovered,
};
use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::{Address, B256, Signature, U256, address, b256, bytes};
use base_common_chains::BaseUpgrade;
use base_common_consensus::{
    BaseReceipt, BaseTransactionSigned, Predeploys, SystemAddresses, TxDeposit,
};
use base_common_evm2::BaseTime;
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use base_protocol::BaseTimeUpdateTx;
use evm2::{AccountInfo, bytecode::Bytecode, evm::InMemoryDB};
use reth_chainspec::{EthereumHardfork, ForkCondition, MIN_TRANSACTION_GAS};
use reth_evm::{
    BlockBuilder, BlockExecutionError, BlockExecutionOutput, BlockValidationError, ConfigureEvm,
    StateUpdate,
    execute::{BasicBlockExecutor, Executor},
};
use reth_primitives_traits::{RecoveredBlock, SealedHeader};
use reth_storage_api::noop::NoopProvider;

const BASE_TIME_READER: Address = address!("0x1000000000000000000000000000000000000000");
const USER: Address = address!("0x1000000000000000000000000000000000000001");
const BLOCK_TIMESTAMP: u64 = 1_725;
const PREVIOUS_MILLIS_PART: u16 = 200;
const CURRENT_MILLIS_PART: u16 = 600;

fn create_base_state_provider() -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &Predeploys::L1_BLOCK_INFO,
        AccountInfo { nonce: 1, ..Default::default() },
    );
    for (slot, value) in [
        // base fee
        (1, U256::from(1000000000)),
        // l1 fee overhead
        (5, U256::from(188)),
        // l1 fee scalar
        (6, U256::from(684000)),
        // l1 free scalars post ecotone
        (
            3,
            U256::from_str("0x0000000000000000000000000000000000001db0000d27300000000000000005")
                .unwrap(),
        ),
    ] {
        db.insert_account_storage(&Predeploys::L1_BLOCK_INFO, &U256::from(slot), &value);
    }
    db
}

fn insert_account(
    db: &mut InMemoryDB,
    address: Address,
    info: AccountInfo,
    code: Option<alloy_primitives::Bytes>,
) {
    let code = code.map(Bytecode::new_raw);
    db.insert_account_info(&address, AccountInfo { code, ..info });
}

fn funded() -> AccountInfo {
    AccountInfo { balance: U256::MAX, ..Default::default() }
}

fn evm_config(chain_spec: Arc<BaseChainSpec>) -> BaseEvmConfig {
    BaseEvmConfig::new(chain_spec)
}

fn execute_same_block_base_time_read(getter_selector: [u8; 4]) -> U256 {
    let mut db = create_base_state_provider();
    insert_account(
        &mut db,
        Predeploys::BASE_TIME,
        AccountInfo::default(),
        Some(BaseTime::proxy_bytecode()),
    );
    for (slot, value) in [
        (BaseTime::ADMIN_SLOT, U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice())),
        (BaseTime::TIMESTAMP_MILLIS_PART_SLOT, U256::from(PREVIOUS_MILLIS_PART)),
    ] {
        db.insert_account_storage(&Predeploys::BASE_TIME, &slot, &value);
    }
    insert_account(
        &mut db,
        BASE_TIME_READER,
        AccountInfo::default(),
        // Forward the four-byte calldata to BaseTime and store its returned word in slot zero.
        Some(bytes!(
            "6004600060003760206000600460007342000000000000000000000000000000000000305afa5060005160005500"
        )),
    );
    insert_account(&mut db, USER, funded(), None);

    let chain_spec = Arc::new(
        BaseChainSpecBuilder::base_mainnet()
            .cobalt_activated()
            .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(0))
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
    let output = BasicBlockExecutor::new(evm_config(chain_spec), db)
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
    insert_account(&mut db, addr, funded(), None);

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

    let executor = BasicBlockExecutor::new(evm_config(chain_spec), db);

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
    insert_account(&mut db, addr, funded(), None);

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

    let executor = BasicBlockExecutor::new(evm_config(chain_spec), db);

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

    // The standard transaction paid an L1 data fee priced from the L1Block predeploy.
    let l1_fee_vault = output.state.account(&Predeploys::L1_FEE_VAULT).and_then(|a| a.info.clone());
    assert!(l1_fee_vault.is_some_and(|info| info.balance > U256::ZERO));
}

/// Executes block 1 at `timestamp` holding one self-transfer from the zero address, whose account
/// is `sender`, with `nonce`.
fn execute_transfer(
    chain_spec: BaseChainSpec,
    timestamp: u64,
    sender: AccountInfo,
    nonce: u64,
    state_hook: impl FnMut(StateUpdate) + Send + 'static,
) -> Result<BlockExecutionOutput<BaseReceipt>, BlockExecutionError> {
    let mut db = create_base_state_provider();
    insert_account(&mut db, Address::ZERO, sender, None);
    let tx: BaseTransactionSigned = TxEip1559 {
        chain_id: chain_spec.chain.id(),
        nonce,
        gas_limit: MIN_TRANSACTION_GAS,
        to: Address::ZERO.into(),
        ..Default::default()
    }
    .into_signed(Signature::test_signature())
    .into();
    let header = Header { timestamp, number: 1, gas_limit: 1_000_000, ..Default::default() };
    BasicBlockExecutor::new(evm_config(Arc::new(chain_spec)), db).execute_with_state_hook(
        &RecoveredBlock::new_unhashed(
            Block { header, body: BlockBody { transactions: vec![tx], ..Default::default() } },
            vec![Address::ZERO],
        ),
        state_hook,
    )
}

#[test]
fn state_hook_streams_transition_and_transaction_changes() {
    const CREATE2_DEPLOYER: Address = address!("0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2");
    // Canyon activates at this block, so it force-deploys the create2 deployer before the
    // transaction runs.
    let chain_spec = BaseChainSpecBuilder::base_mainnet()
        .regolith_activated()
        .with_fork(EthereumHardfork::Shanghai, ForkCondition::Timestamp(2))
        .with_fork(BaseUpgrade::Canyon, ForkCondition::Timestamp(2))
        .build();
    let updated = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&updated);

    execute_transfer(chain_spec, 2, funded(), 0, move |update| {
        sink.lock().unwrap().extend(update.accounts.iter().map(|account| account.address));
    })
    .expect("block executes");

    let updated = updated.lock().unwrap();
    assert!(updated.contains(&CREATE2_DEPLOYER), "transition change is streamed");
    assert!(updated.contains(&Address::ZERO), "transaction change is streamed");
}

#[test]
fn stale_nonce_is_an_invalid_transaction() {
    let chain_spec = BaseChainSpecBuilder::base_mainnet().canyon_activated().build();
    let sender = AccountInfo { nonce: 1, ..funded() };

    let error = execute_transfer(chain_spec, 2, sender, 0, |_| {}).unwrap_err();

    let BlockExecutionError::Validation(BlockValidationError::InvalidTx { error, .. }) = error
    else {
        panic!("expected an invalid transaction, got {error:?}");
    };
    assert!(error.is_nonce_too_low());
}

#[test]
fn built_jovian_block_records_the_da_footprint_and_isthmus_fields() {
    let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().jovian_activated().build());
    let mut db = create_base_state_provider();
    // DA-footprint gas scalar 400, packed at byte offset 18 of the operator-fee slot.
    db.insert_account_storage(&Predeploys::L1_BLOCK_INFO, &U256::from(8), &(U256::from(400) << 96));
    insert_account(&mut db, USER, funded(), None);
    let parent = SealedHeader::seal_slow(Header {
        number: 1,
        timestamp: 1,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(1),
        ..Default::default()
    });
    let attributes = BaseNextBlockEnvAttributes {
        timestamp: 3,
        suggested_fee_recipient: Address::repeat_byte(0x33),
        prev_randao: B256::repeat_byte(0x77),
        gas_limit: 30_000_000,
        parent_beacon_block_root: Some(B256::ZERO),
        extra_data: Default::default(),
    };
    let l1_info_tx: BaseTransactionSigned = TxDeposit {
        from: SystemAddresses::DEPOSITOR_ACCOUNT,
        to: Predeploys::L1_BLOCK_INFO.into(),
        gas_limit: 1_000_000,
        ..Default::default()
    }
    .into();
    let user_tx: BaseTransactionSigned = TxEip1559 {
        chain_id: chain_spec.chain.id(),
        gas_limit: 100_000,
        max_fee_per_gas: 1_000,
        to: USER.into(),
        ..Default::default()
    }
    .into_signed(Signature::test_signature())
    .into();

    let evm_config = evm_config(chain_spec);
    let mut builder = evm_config.builder_for_next_block(db, &parent, attributes).unwrap();
    builder.apply_pre_execution_changes().unwrap();
    builder
        .execute_transaction(Recovered::new_unchecked(
            l1_info_tx,
            SystemAddresses::DEPOSITOR_ACCOUNT,
        ))
        .unwrap();
    builder.execute_transaction(Recovered::new_unchecked(user_tx, USER)).unwrap();
    let outcome = builder.finish(NoopProvider::default(), None).unwrap();

    let header = outcome.block.header();
    assert!(outcome.execution_result.blob_gas_used > 0, "the user transaction has a DA footprint");
    assert_eq!(header.blob_gas_used, Some(outcome.execution_result.blob_gas_used));
    assert_eq!(header.excess_blob_gas, Some(0));
    assert_eq!(header.requests_hash, Some(EMPTY_REQUESTS_HASH));
    assert_eq!(header.withdrawals_root, Some(B256::ZERO), "the message passer's storage root");
    assert_eq!(header.mix_hash, B256::repeat_byte(0x77));
    assert_eq!(header.gas_used, outcome.execution_result.gas_used);
    assert_eq!(outcome.block.body().withdrawals.as_ref().map(|w| w.len()), Some(0));
}
