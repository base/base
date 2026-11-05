//! Block-level differential parity harness.
//!
//! Runs a multi-transaction block through the `base-common-evm2` [`BaseBlockExecutor`] and, in the
//! same order, through the revm-based `base-common-evm` reference (committing between transactions
//! so nonces and balances carry), asserting the two engines agree on the block-level accounting
//! the executor is responsible for: each transaction's success and the block's cumulative gas.
//!
//! Per-transaction fee distribution is separately covered by `standard_fee_parity`; this harness
//! focuses on the executor's running cumulative-gas accounting and transaction ordering across a
//! block that, like every L2 block, opens with a deposit, followed by several transactions from
//! the same sender.

use alloy_consensus::{TxEip1559, transaction::Recovered};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use base_common_consensus::{BaseReceipt, TxDeposit};
use base_common_evm::{
    BaseSpecId as RevmBaseSpecId, BaseTransaction, Builder, DefaultBase, L1BlockInfo,
};
use base_common_evm2::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseEvmExt, BaseEvmTypes, BaseSpecId, BaseTxEnvelope,
};
use base_common_genesis::BaseUpgrade;
use base_common_l1_fees::L1FeeParams;
use evm2::{Evm, Precompiles, env::BlockEnv, ethereum::TxEnvelope, evm::InMemoryDB};
use revm::{
    ExecuteCommitEvm,
    context::{BlockEnv as RevmBlockEnv, CfgEnv, Context, TxEnv},
    database::InMemoryDB as RevmDb,
    state::AccountInfo as RevmAccountInfo,
};

const SENDER: Address = Address::repeat_byte(0x11);
const DEPOSITOR: Address = Address::repeat_byte(0x44);
const DEPOSIT_MINT: u128 = 1_000_000;
const DEPOSIT_VALUE: u64 = 1_000;
const DEPOSIT_GAS_LIMIT: u64 = 50_000;
const TARGET: Address = Address::repeat_byte(0x22);
const COINBASE: Address = Address::repeat_byte(0x33);
const CHAIN_ID: u64 = 1;
const GAS_LIMIT: u64 = 100_000;
const MAX_FEE: u128 = 1_000;
const PRIORITY_FEE: u128 = 100;
const BASEFEE: u64 = 500;
const BLOCK_GAS_LIMIT: u64 = 30_000_000;
/// Number of standard transactions in the test block after the deposit (from `SENDER`, ascending
/// nonces).
const TX_COUNT: u64 = 3;

/// The whole execution fork ladder.
const FORKS: [BaseUpgrade; 14] = BaseUpgrade::EXECUTION_VARIANTS;

fn enveloped() -> Bytes {
    Bytes::from(vec![0x02u8; 120])
}

fn l1_fee_params() -> L1FeeParams {
    L1FeeParams {
        l1_base_fee: U256::from(1_000_000_000u64),
        l1_base_fee_scalar: U256::from(1_000u64),
        operator_fee_scalar: Some(U256::from(2_000u64)),
        operator_fee_constant: Some(U256::from(7u64)),
        ..Default::default()
    }
}

/// Marks the block environment's L1 fee parameters as already loaded for block 0, like the revm
/// reference's preloaded `L1BlockInfo { l2_block: Some(0), .. }`, so execution prices fees from
/// them instead of reading the (empty) `L1Block` predeploy.
fn preloaded_l1_fee_params() -> BaseEvmExt {
    BaseEvmExt { l1_fee_params_block: Some(U256::ZERO), ..Default::default() }
}

#[derive(Debug, PartialEq, Eq)]
struct BlockOutcome {
    successes: Vec<bool>,
    cumulative_gas: u64,
}

/// Runs the deposit and the `TX_COUNT` transfers through the evm2 block executor.
fn run_evm2(upgrade: BaseUpgrade) -> BlockOutcome {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &SENDER,
        evm2::AccountInfo { balance: U256::from(10u128.pow(18)), nonce: 0, ..Default::default() },
    );
    let spec = BaseSpecId::new(upgrade);
    let block = BlockEnv::<BaseEvmTypes> {
        beneficiary: COINBASE,
        basefee: U256::from(BASEFEE),
        gas_limit: U256::from(BLOCK_GAS_LIMIT),
        ext: l1_fee_params(),
        ..Default::default()
    };
    let evm =
        Evm::new(spec, block, BaseEvmTypes::tx_registry(), db, Precompiles::base(spec.into()))
            .with_ext(preloaded_l1_fee_params());
    let mut executor = BaseBlockExecutor::new(evm, BaseBlockExecutionCtx::default());

    let deposit = BaseTxEnvelope::Deposit(TxDeposit {
        source_hash: B256::repeat_byte(0xab),
        from: DEPOSITOR,
        to: TxKind::Call(TARGET),
        mint: DEPOSIT_MINT,
        value: U256::from(DEPOSIT_VALUE),
        gas_limit: DEPOSIT_GAS_LIMIT,
        is_system_transaction: false,
        input: Bytes::new(),
    });
    executor
        .execute_transaction(&Recovered::new_unchecked(deposit, DEPOSITOR))
        .expect("deposit executes");

    for nonce in 0..TX_COUNT {
        let tx = TxEnvelope::Eip1559(TxEip1559 {
            chain_id: CHAIN_ID,
            nonce,
            gas_limit: GAS_LIMIT,
            max_fee_per_gas: MAX_FEE,
            max_priority_fee_per_gas: PRIORITY_FEE,
            to: TxKind::Call(TARGET),
            value: U256::from(1),
            input: Bytes::new(),
            access_list: Default::default(),
        });
        let envelope = BaseTxEnvelope::standard(tx, enveloped());
        executor
            .execute_transaction(&Recovered::new_unchecked(envelope, SENDER))
            .expect("tx executes");
    }

    let (_evm, result, _) = executor.finish();
    let successes = result
        .receipts
        .iter()
        .map(|r| match r {
            BaseReceipt::Deposit(r) => r.inner.status.coerce_status(),
            BaseReceipt::Eip1559(r) => r.status.coerce_status(),
            other => panic!("unexpected receipt variant: {other:?}"),
        })
        .collect();
    BlockOutcome { successes, cumulative_gas: result.gas_used }
}

/// Runs the same block through the revm reference, committing between transactions.
fn run_revm(upgrade: BaseUpgrade) -> BlockOutcome {
    let mut db = RevmDb::default();
    db.insert_account_info(
        SENDER,
        RevmAccountInfo { balance: U256::from(10u128.pow(18)), nonce: 0, ..Default::default() },
    );
    let params = l1_fee_params();
    let ctx = Context::base()
        .with_db(db)
        .with_chain(L1BlockInfo {
            l2_block: Some(U256::ZERO),
            l1_base_fee: params.l1_base_fee,
            l1_base_fee_scalar: params.l1_base_fee_scalar,
            operator_fee_scalar: params.operator_fee_scalar,
            operator_fee_constant: params.operator_fee_constant,
            ..Default::default()
        })
        .with_block(RevmBlockEnv {
            basefee: BASEFEE,
            beneficiary: COINBASE,
            gas_limit: BLOCK_GAS_LIMIT,
            ..Default::default()
        })
        .with_cfg(CfgEnv::new_with_spec(RevmBaseSpecId::new(upgrade)));
    let mut evm = ctx.build_base();

    let deposit = BaseTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(DEPOSITOR)
                .kind(TxKind::Call(TARGET))
                .gas_limit(DEPOSIT_GAS_LIMIT)
                .value(U256::from(DEPOSIT_VALUE)),
        )
        .source_hash(B256::repeat_byte(0xab))
        .mint(DEPOSIT_MINT)
        .build_fill();
    let result = evm.transact_commit(deposit).expect("deposit executes");
    let mut successes = vec![result.is_success()];
    let mut cumulative_gas = result.tx_gas_used();
    for nonce in 0..TX_COUNT {
        let tx = BaseTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(SENDER)
                    .nonce(nonce)
                    .chain_id(Some(CHAIN_ID))
                    .kind(TxKind::Call(TARGET))
                    .gas_limit(GAS_LIMIT)
                    .max_fee_per_gas(MAX_FEE)
                    .gas_priority_fee(Some(PRIORITY_FEE))
                    .value(U256::from(1)),
            )
            .enveloped_tx(Some(enveloped()))
            .build_fill();
        let result = evm.transact_commit(tx).expect("tx executes");
        successes.push(result.is_success());
        cumulative_gas += result.tx_gas_used();
    }

    BlockOutcome { successes, cumulative_gas }
}

#[test]
fn block_cumulative_gas_and_success_match_revm() {
    for upgrade in FORKS {
        let evm2 = run_evm2(upgrade);
        let revm = run_revm(upgrade);
        assert_eq!(evm2.successes.len(), TX_COUNT as usize + 1);
        assert!(evm2.successes.iter().all(|&s| s), "all txs succeed at {upgrade:?}");
        assert_eq!(evm2, revm, "block cumulative gas / success diverged at {upgrade:?}");
    }
}
