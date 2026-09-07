//! Executing transactions independently against an already-built EIP-7928 Block
//! Access List (BAL).
//!
//! With a BAL attached, reads are served from it at the transaction's block access
//! index (`i + 1` for transaction `i`), so transaction `i` sees the post-state of
//! transactions `0..i` without those transactions having been committed to this
//! EVM. Every execution only needs the pre-block database plus the shared BAL,
//! which is what makes the transactions of a block executable in parallel.
//!
//! Each worker thread detaches its transaction's pending state -- the transaction
//! overlay moved out of the EVM -- and sends it back to the main thread, which folds
//! it into a fresh BAL and checks it against the block's BAL: the validation half of
//! EIP-7928. The pending state still streams the change-set persistence consumers
//! (e.g. reth) apply to the database.

use std::sync::Arc;

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_eip7928::BalanceChange;
use alloy_primitives::{Address, TxKind, U256};
use evm2::{
    BaseEvmTypes, Evm, Precompiles, SpecId, TxResultWithState,
    env::BlockEnvExt,
    ethereum::{RecoveredTxEnvelope, TxEnvelope, ethereum_tx_registry},
    evm::{AccountInfo, Bal, BlockAccessIndex, InMemoryDB},
};

const CALLER: Address = Address::with_last_byte(0xaa);
const ALICE: Address = Address::with_last_byte(0xbb);
const BOB: Address = Address::with_last_byte(0xcc);

fn main() {
    // Transaction 1 spends money ALICE only receives in transaction 0, so it cannot
    // execute against pre-block state alone.
    let transactions = [transfer(CALLER, ALICE, 1_000_000, 0), transfer(ALICE, BOB, 400_000, 0)];

    // Sequential execution with the builder enabled produces the block's BAL (see the
    // `bal_build` example). A validator would instead decode it from the block body.
    let bal = build_bal(&transactions);
    assert_eq!(
        bal.accounts.get(&ALICE).unwrap().account_info.balance.changes,
        vec![
            BalanceChange::new(idx(1), U256::from(1_000_000)),
            BalanceChange::new(idx(2), U256::from(600_000)),
        ]
    );

    // Without the BAL, transaction 1 on pre-block state fails: ALICE has no funds yet.
    let mut evm = pre_block_evm();
    assert!(evm.transact(&transactions[1]).is_err());

    // With the BAL attached, each transaction executes on its own EVM over the same
    // pre-block database, one worker thread per transaction. Reads positioned at
    // index `i + 1` see all writes recorded at indices `<= i`, i.e. exactly the
    // transaction's pre-state, so no thread needs another thread's committed state.
    //
    // Each worker detaches its transaction into an owned result + pending state and
    // sends both back to main through its join handle. Main is the single consumer:
    // it folds each pending state into a fresh BAL in transaction order (`Bal` writes
    // must be appended with ascending indices) and checks the rebuilt BAL against the
    // block's BAL, which is exactly how a validator confirms the block's BAL is
    // correct.
    let bal = Arc::new(bal);
    let mut bal_builder = Bal::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = transactions
            .iter()
            .enumerate()
            .map(|(i, tx)| {
                let bal = bal.clone();
                s.spawn(move || execute_with_bal(bal, i as u64 + 1, tx))
            })
            .collect();
        for (i, handle) in handles.into_iter().enumerate() {
            let TxResultWithState { result, pending_state: pending, .. } =
                handle.join().expect("worker thread panicked");
            assert!(result.status);

            // The pending state still carries the change-set that persistence
            // consumers (e.g. reth) stream to the database through a
            // `StateChangeSink`; detaching it for the BAL fold loses nothing.
            bal_builder.commit(idx(i as u64 + 1), pending);
            println!("transaction {i} executed in parallel, gas used {}", result.tx_gas_used());
        }
    });
    assert_eq!(bal_builder, *bal, "BAL rebuilt from parallel execution must match the block's BAL");
    println!("rebuilt BAL from parallel outputs matches the block's BAL");
}

/// Executes one transaction over pre-block state with reads served from `bal` at
/// `index`. A read the BAL does not cover returns `ErrorCode::BAL_NOT_COVERED`,
/// which during validation means the BAL is invalid (use
/// `set_allow_bal_db_fallback(true)` to instead fall through to the database, e.g.
/// for RPC calls on BAL-positioned state).
///
/// The executed transaction is detached into an owned [`TxResultWithState`] so it
/// can leave the worker thread; nothing is committed to this EVM, which is dropped
/// here.
fn execute_with_bal(bal: Arc<Bal>, index: u64, tx: &RecoveredTxEnvelope) -> TxResultWithState {
    let mut evm = pre_block_evm();
    evm.state_mut().set_bal(bal);
    evm.state_mut().set_bal_index(BlockAccessIndex::new(index));
    evm.transact(tx).expect("transaction should execute").detach()
}

fn build_bal(transactions: &[RecoveredTxEnvelope]) -> Bal {
    let mut evm = pre_block_evm();
    evm.state_mut().enable_bal_builder();
    evm.state_mut().reset_bal_index();
    for tx in transactions {
        evm.state_mut().bump_bal_index();
        let result = evm.transact(tx).expect("transaction should execute").commit();
        assert!(result.status);
    }
    evm.state_mut().take_bal_builder().expect("builder was enabled")
}

fn pre_block_evm() -> Evm<'static, BaseEvmTypes> {
    let mut database = InMemoryDB::default();
    database.insert_account_info(
        &CALLER,
        AccountInfo::default().with_balance(U256::from(1_000_000_000_u64)),
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

const fn idx(index: u64) -> BlockAccessIndex {
    BlockAccessIndex::new(index)
}
