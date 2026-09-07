//! Building an EIP-7928 Block Access List (BAL) during sequential block execution.
//!
//! The block access index follows the EIP-7928 layout: index 0 for pre-block system
//! calls, `i + 1` for transaction `i`, and one final index for the post-block
//! transition (system calls, rewards, withdrawals). The caller drives the index:
//! reset it before the block, then bump it once per transaction and once more before
//! the post-block step. [`Evm::system_call`] and [`Evm::transact`] record each
//! committed post-state into the builder at the current index automatically.

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_eip7928::{BalanceChange, BlockAccessList, NonceChange, StorageChange};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use evm2::{
    BaseEvmTypes, Evm, Precompiles, SpecId,
    bytecode::Bytecode,
    env::BlockEnvExt,
    ethereum::{RecoveredTxEnvelope, TxEnvelope, ethereum_tx_registry},
    evm::{AccountInfo, BlockAccessIndex, InMemoryDB, SystemTx},
    interpreter::op,
};

const CALLER: Address = Address::with_last_byte(0xaa);
const ALICE: Address = Address::with_last_byte(0xbb);
const STORAGE_CONTRACT: Address = Address::with_last_byte(0xcc);
const PRE_SYSTEM_CONTRACT: Address = Address::with_last_byte(0xf1);
const POST_SYSTEM_CONTRACT: Address = Address::with_last_byte(0xf2);

fn main() {
    let mut evm = block_evm();

    // Enable BAL construction and position the index at the pre-block slot (index 0).
    evm.state_mut().enable_bal_builder();
    evm.state_mut().reset_bal_index();
    assert_eq!(evm.state_mut().bal_index(), idx(0));

    // Pre-block system call (think EIP-4788 / EIP-2935), recorded at index 0.
    let executed = evm
        .system_call(SystemTx::new(PRE_SYSTEM_CONTRACT, word(0xbeef)))
        .expect("pre-block system call should execute");
    assert!(executed.result().status, "pre-block system call failed: {:?}", executed.result());
    let _ = executed.commit();

    // Transaction 0 is recorded at index 1: a plain value transfer to ALICE.
    evm.state_mut().bump_bal_index();
    assert_eq!(evm.state_mut().bal_index(), idx(1));
    let tx0 = transfer(CALLER, ALICE, 1_000_000, 0);
    let result = evm.transact(&tx0).expect("transaction 0 should execute").commit();
    assert!(result.status, "transaction 0 failed: {result:?}");

    // Transaction 1 is recorded at index 2: a call that stores 42 at slot 5.
    evm.state_mut().bump_bal_index();
    assert_eq!(evm.state_mut().bal_index(), idx(2));
    let tx1 = Recovered::new_unchecked(
        TxEnvelope::Legacy(TxLegacy {
            to: TxKind::Call(STORAGE_CONTRACT),
            input: word(42),
            gas_limit: 300_000,
            nonce: 1,
            ..Default::default()
        }),
        CALLER,
    );
    let result = evm.transact(&tx1).expect("transaction 1 should execute").commit();
    assert!(result.status);

    // The post-block transition is recorded at the final index 3. A system call
    // commits through the same path as a transaction; block rewards and withdrawals,
    // which bypass transaction commit, would instead be recorded here through
    // `overlay_db_mut().bal_context.commit_account_change(..)`.
    evm.state_mut().bump_bal_index();
    assert_eq!(evm.state_mut().bal_index(), idx(3));
    let executed = evm
        .system_call(SystemTx::new(POST_SYSTEM_CONTRACT, word(0x22)))
        .expect("post-block system call should execute");
    assert!(executed.result().status);
    let _ = executed.commit();

    // Take the built BAL. Writes carry the block access index they happened at.
    let bal = evm.state_mut().take_bal_builder().expect("builder was enabled");

    let pre_system = bal.accounts.get(&PRE_SYSTEM_CONTRACT).unwrap();
    assert_eq!(
        pre_system.storage.storage.get(&U256::ZERO).unwrap().changes,
        vec![StorageChange::new(idx(0), U256::from(0xbeef))]
    );

    let caller = bal.accounts.get(&CALLER).unwrap();
    assert_eq!(
        caller.account_info.nonce.changes,
        vec![NonceChange::new(idx(1), 1), NonceChange::new(idx(2), 2)]
    );

    let alice = bal.accounts.get(&ALICE).unwrap();
    assert_eq!(
        alice.account_info.balance.changes,
        vec![BalanceChange::new(idx(1), U256::from(1_000_000))]
    );

    let storage_contract = bal.accounts.get(&STORAGE_CONTRACT).unwrap();
    assert_eq!(
        storage_contract.storage.storage.get(&U256::from(5)).unwrap().changes,
        vec![StorageChange::new(idx(2), U256::from(42))]
    );

    let post_system = bal.accounts.get(&POST_SYSTEM_CONTRACT).unwrap();
    assert_eq!(
        post_system.storage.storage.get(&U256::ZERO).unwrap().changes,
        vec![StorageChange::new(idx(3), U256::from(0x22))]
    );

    println!("{bal}");

    // Converting into `BlockAccessList` canonicalizes the builder into the EIP-7928 wire shape
    // (accounts and nested changes in deterministic order) for hashing/encoding.
    let alloy_bal = BlockAccessList::from(bal);
    println!("canonical EIP-7928 list covers {} accounts", alloy_bal.len());
}

fn block_evm() -> Evm<'static, BaseEvmTypes> {
    let mut database = InMemoryDB::default();
    database.insert_account_info(
        &CALLER,
        AccountInfo::default().with_balance(U256::from(1_000_000_000_u64)),
    );
    database.insert_account_info(
        &PRE_SYSTEM_CONTRACT,
        AccountInfo::default().with_code(Bytecode::new_legacy(store_calldata_code(0))),
    );
    database.insert_account_info(
        &POST_SYSTEM_CONTRACT,
        AccountInfo::default().with_code(Bytecode::new_legacy(store_calldata_code(0))),
    );
    database.insert_account_info(
        &STORAGE_CONTRACT,
        AccountInfo::default().with_code(Bytecode::new_legacy(store_calldata_code(5))),
    );

    let spec = SpecId::AMSTERDAM;
    Evm::new(
        spec,
        BlockEnvExt::default(),
        ethereum_tx_registry(spec),
        database,
        Precompiles::base(spec),
    )
}

fn transfer(from: Address, to: Address, value: u64, nonce: u64) -> RecoveredTxEnvelope {
    Recovered::new_unchecked(
        TxEnvelope::Legacy(TxLegacy {
            to: TxKind::Call(to),
            value: U256::from(value),
            gas_limit: 300_000,
            nonce,
            ..Default::default()
        }),
        from,
    )
}

/// Stores the first calldata word at storage slot `slot`.
fn store_calldata_code(slot: u8) -> Bytes {
    vec![op::PUSH0, op::CALLDATALOAD, op::PUSH1, slot, op::SSTORE, op::STOP].into()
}

fn word(value: u64) -> Bytes {
    U256::from(value).to_be_bytes_vec().into()
}

const fn idx(index: u64) -> BlockAccessIndex {
    BlockAccessIndex::new(index)
}
