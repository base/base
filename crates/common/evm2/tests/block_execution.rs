//! Block-executor tests for `base-common-evm2`.
//!
//! Runs a block of a deposit followed by a standard EIP-1559 transaction through the
//! [`BaseBlockExecutor`] and asserts the receipts (types, status, cumulative gas, deposit
//! nonce/version) and post-block state. Per-transaction execution is separately proven equal to
//! the revm reference by the deposit and standard-fee parity harnesses, so this test focuses on
//! the block-executor additions: receipt building, cumulative gas, and block-state commit.

use alloy_consensus::{TxEip1559, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use base_common_consensus::{BaseReceiptEnvelope, Predeploys, TxDeposit};
use base_common_evm2::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseEvmTypes, BaseSpecId, BaseTxEnvelope,
};
use base_common_genesis::BaseUpgrade;
use base_common_l1_fees::L1FeeParams;
use evm2::{Evm, Precompiles, env::BlockEnv, ethereum::TxEnvelope, evm::InMemoryDB};

const SENDER: Address = Address::repeat_byte(0x11);
const TARGET: Address = Address::repeat_byte(0x22);
const COINBASE: Address = Address::repeat_byte(0x33);

const fn cumulative_gas(receipt: &BaseReceiptEnvelope) -> u64 {
    match receipt {
        BaseReceiptEnvelope::Deposit(r) => r.receipt.inner.cumulative_gas_used,
        BaseReceiptEnvelope::Legacy(r)
        | BaseReceiptEnvelope::Eip2930(r)
        | BaseReceiptEnvelope::Eip1559(r)
        | BaseReceiptEnvelope::Eip7702(r)
        | BaseReceiptEnvelope::Eip8130(r) => r.receipt.cumulative_gas_used,
    }
}

fn nonce(evm: &mut Evm<'static, BaseEvmTypes>, addr: Address) -> u64 {
    evm.state_mut().account_info_untracked(&addr).unwrap().map(|i| i.nonce).unwrap_or_default()
}

fn balance(evm: &mut Evm<'static, BaseEvmTypes>, addr: Address) -> U256 {
    evm.state_mut().account_info_untracked(&addr).unwrap().map(|i| i.balance).unwrap_or_default()
}

#[test]
fn executes_block_of_deposit_then_standard_tx() {
    let spec = BaseSpecId::new(BaseUpgrade::Ecotone);
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &SENDER,
        evm2::AccountInfo { balance: U256::from(10u128.pow(18)), nonce: 0, ..Default::default() },
    );
    let block = BlockEnv::<BaseEvmTypes> {
        beneficiary: COINBASE,
        basefee: U256::from(500),
        ext: L1FeeParams {
            l1_base_fee: U256::from(1_000_000_000u64),
            l1_base_fee_scalar: U256::from(1_000u64),
            ..Default::default()
        },
        ..Default::default()
    };
    let evm =
        Evm::new(spec, block, BaseEvmTypes::tx_registry(), db, Precompiles::base(spec.into()));
    let mut executor = BaseBlockExecutor::new(evm, BaseBlockExecutionCtx::default());

    // A deposit that mints to the sender, then a standard EIP-1559 transfer from the same sender.
    let deposit = TxDeposit {
        from: SENDER,
        to: TxKind::Call(TARGET),
        mint: 1_000,
        gas_limit: 100_000,
        ..Default::default()
    };
    executor
        .execute_transaction(&Recovered::new_unchecked(BaseTxEnvelope::Deposit(deposit), SENDER))
        .expect("deposit executes");

    let standard = TxEnvelope::Eip1559(TxEip1559 {
        chain_id: 1,
        nonce: 1,
        gas_limit: 100_000,
        max_fee_per_gas: 1_000,
        max_priority_fee_per_gas: 100,
        to: TxKind::Call(TARGET),
        value: U256::from(10),
        input: Bytes::new(),
        access_list: Default::default(),
    });
    let enveloped = Bytes::from(vec![0x02u8; 120]);
    executor
        .execute_transaction(&Recovered::new_unchecked(
            BaseTxEnvelope::standard(standard, enveloped),
            SENDER,
        ))
        .expect("standard tx executes");

    let (mut evm, result, _block_state) = executor.finish();

    // Two receipts, in order, of the expected variants.
    assert_eq!(result.receipts.len(), 2);
    match &result.receipts[0] {
        BaseReceiptEnvelope::Deposit(r) => {
            assert!(r.receipt.inner.status.coerce_status(), "deposit succeeded");
            // Pre-execution sender nonce (0), and Canyon-gated receipt version (Ecotone > Canyon).
            assert_eq!(r.receipt.deposit_nonce, Some(0));
            assert_eq!(r.receipt.deposit_receipt_version, Some(1));
        }
        other => panic!("expected deposit receipt, got {other:?}"),
    }
    assert!(
        matches!(result.receipts[1], BaseReceiptEnvelope::Eip1559(_)),
        "second receipt is EIP-1559"
    );

    // Cumulative gas is monotonic and the block gas equals the last receipt's cumulative gas.
    assert!(cumulative_gas(&result.receipts[0]) < cumulative_gas(&result.receipts[1]));
    assert_eq!(result.gas_used, cumulative_gas(&result.receipts[1]));

    // Both transactions bumped the sender nonce (deposit → 1, standard → 2).
    assert_eq!(nonce(&mut evm, SENDER), 2);
    // The standard transaction's L1 data fee was collected into the vault (the deposit is exempt).
    assert!(balance(&mut evm, Predeploys::L1_FEE_VAULT) > U256::ZERO);
}

/// Builds an executor over a block whose gas limit is `block_gas_limit`, with `SENDER` funded.
fn executor_with_block_gas_limit(
    spec: BaseSpecId,
    block_gas_limit: u64,
) -> BaseBlockExecutor<'static> {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &SENDER,
        evm2::AccountInfo { balance: U256::from(10u128.pow(18)), nonce: 0, ..Default::default() },
    );
    let block = BlockEnv::<BaseEvmTypes> {
        beneficiary: COINBASE,
        gas_limit: U256::from(block_gas_limit),
        ..Default::default()
    };
    let evm =
        Evm::new(spec, block, BaseEvmTypes::tx_registry(), db, Precompiles::base(spec.into()));
    BaseBlockExecutor::new(evm, BaseBlockExecutionCtx::default())
}

#[test]
fn rejects_standard_tx_over_block_gas_limit() {
    // Block allows 50k gas; the transaction reserves its 100k gas limit.
    let mut executor = executor_with_block_gas_limit(BaseSpecId::new(BaseUpgrade::Ecotone), 50_000);
    let standard = TxEnvelope::Eip1559(TxEip1559 {
        chain_id: 1,
        nonce: 0,
        gas_limit: 100_000,
        max_fee_per_gas: 1_000,
        max_priority_fee_per_gas: 100,
        to: TxKind::Call(TARGET),
        value: U256::ZERO,
        input: Bytes::new(),
        access_list: Default::default(),
    });
    let err = executor
        .execute_transaction(&Recovered::new_unchecked(
            BaseTxEnvelope::standard(standard, Bytes::from(vec![0x02u8; 120])),
            SENDER,
        ))
        .expect_err("tx over the block gas limit is rejected");
    assert!(format!("{err}").contains("more than the block's available gas"), "got: {err}");
}

#[test]
fn rejects_post_regolith_deposit_over_block_gas_limit() {
    // Post-Regolith deposits ARE subject to the block-gas check.
    let mut executor =
        executor_with_block_gas_limit(BaseSpecId::new(BaseUpgrade::Regolith), 50_000);
    let deposit = TxDeposit {
        from: SENDER,
        to: TxKind::Call(TARGET),
        gas_limit: 100_000,
        ..Default::default()
    };
    executor
        .execute_transaction(&Recovered::new_unchecked(BaseTxEnvelope::Deposit(deposit), SENDER))
        .expect_err("post-Regolith deposit over the block gas limit is rejected");
}

/// Builds a Jovian executor with the given block gas limit and DA-footprint gas scalar.
fn jovian_executor(block_gas_limit: u64, da_scalar: u128) -> BaseBlockExecutor<'static> {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &SENDER,
        evm2::AccountInfo { balance: U256::from(10u128.pow(18)), nonce: 0, ..Default::default() },
    );
    let block = BlockEnv::<BaseEvmTypes> {
        beneficiary: COINBASE,
        gas_limit: U256::from(block_gas_limit),
        ext: L1FeeParams {
            da_footprint_gas_scalar: Some(U256::from(da_scalar)),
            ..Default::default()
        },
        ..Default::default()
    };
    let spec = BaseSpecId::new(BaseUpgrade::Jovian);
    let evm =
        Evm::new(spec, block, BaseEvmTypes::tx_registry(), db, Precompiles::base(spec.into()));
    BaseBlockExecutor::new(evm, BaseBlockExecutionCtx::default())
}

fn jovian_transfer() -> Recovered<BaseTxEnvelope> {
    let standard = TxEnvelope::Eip1559(TxEip1559 {
        chain_id: 1,
        nonce: 0,
        gas_limit: 100_000,
        max_fee_per_gas: 1_000,
        max_priority_fee_per_gas: 100,
        to: TxKind::Call(TARGET),
        value: U256::ZERO,
        input: Bytes::new(),
        access_list: Default::default(),
    });
    Recovered::new_unchecked(
        BaseTxEnvelope::standard(standard, Bytes::from(vec![0x02u8; 120])),
        SENDER,
    )
}

#[test]
fn jovian_rejects_tx_over_da_footprint_budget() {
    // A large DA scalar makes even a minimal transaction's DA footprint exceed the block budget.
    let mut executor = jovian_executor(60_000_000, 1_000_000);
    let err = executor
        .execute_transaction(&jovian_transfer())
        .expect_err("a tx over the block DA footprint budget is rejected");
    assert!(format!("{err}").contains("DA footprint"), "got: {err}");
}

#[test]
fn jovian_accumulates_blob_gas_used() {
    // A modest DA scalar keeps the footprint within budget; it is surfaced as blob_gas_used.
    let mut executor = jovian_executor(60_000_000, 1_000);
    executor.execute_transaction(&jovian_transfer()).expect("tx within DA budget executes");
    let (_evm, result, _) = executor.finish();
    assert!(result.blob_gas_used > 0, "the DA footprint is surfaced as blob_gas_used");
}

#[test]
fn pre_jovian_block_has_no_da_footprint() {
    // Before Jovian, the DA footprint is not metered and blob_gas_used stays zero.
    let mut executor =
        executor_with_block_gas_limit(BaseSpecId::new(BaseUpgrade::Isthmus), 60_000_000);
    executor.execute_transaction(&jovian_transfer()).expect("tx executes");
    let (_evm, result, _) = executor.finish();
    assert_eq!(result.blob_gas_used, 0, "no DA footprint before Jovian");
}

#[test]
fn azul_enforces_eip7825_per_tx_gas_cap() {
    // Azul maps to the Osaka EVM spec, which caps a transaction's gas limit at 2^24 (EIP-7825).
    // The block has ample room, so the block-gas pre-check passes and the standard handler's cap
    // check is what rejects the transaction.
    const OSAKA_TX_GAS_CAP: u64 = 1 << 24;
    let mut executor =
        executor_with_block_gas_limit(BaseSpecId::new(BaseUpgrade::Azul), 60_000_000);
    let standard = TxEnvelope::Eip1559(TxEip1559 {
        chain_id: 1,
        nonce: 0,
        gas_limit: OSAKA_TX_GAS_CAP + 1,
        max_fee_per_gas: 1_000,
        max_priority_fee_per_gas: 100,
        to: TxKind::Call(TARGET),
        value: U256::ZERO,
        input: Bytes::new(),
        access_list: Default::default(),
    });
    executor
        .execute_transaction(&Recovered::new_unchecked(
            BaseTxEnvelope::standard(standard, Bytes::from(vec![0x02u8; 120])),
            SENDER,
        ))
        .expect_err("a standard tx over the Osaka per-tx gas cap is rejected at Azul");
}

#[test]
fn azul_exempts_deposits_from_the_per_tx_gas_cap() {
    // Deposits are funded on L1 and are never subject to the EIP-7825 per-tx gas cap.
    const OSAKA_TX_GAS_CAP: u64 = 1 << 24;
    let mut executor =
        executor_with_block_gas_limit(BaseSpecId::new(BaseUpgrade::Azul), 60_000_000);
    let deposit = TxDeposit {
        from: SENDER,
        to: TxKind::Call(TARGET),
        gas_limit: OSAKA_TX_GAS_CAP + 1,
        ..Default::default()
    };
    executor
        .execute_transaction(&Recovered::new_unchecked(BaseTxEnvelope::Deposit(deposit), SENDER))
        .expect("a deposit over the Osaka per-tx gas cap is exempt");
}

#[test]
fn eip8130_block_gas_reservation_includes_payer_auth() {
    use base_common_consensus::{Eip8130Constants, Eip8130Signed, TxEip8130};

    // A payer auth blob (`K1 authenticator || data`) so the payer-auth reservation is non-zero.
    let mut payer_auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
    payer_auth.extend_from_slice(&[0xab; 65]);
    let signed = Eip8130Signed::new(
        TxEip8130 {
            gas_limit: 100_000,
            payer: Some(TARGET),
            sender: Some(SENDER),
            ..Default::default()
        },
        Bytes::from(Eip8130Constants::K1_AUTHENTICATOR.to_vec()),
        Bytes::from(payer_auth),
    );
    // The block admits the 100k gas limit alone, but not once the payer-auth reservation is added.
    let mut executor = executor_with_block_gas_limit(BaseSpecId::new(BaseUpgrade::Cobalt), 105_000);
    let err = executor
        .execute_transaction(&Recovered::new_unchecked(BaseTxEnvelope::Eip8130(signed), SENDER))
        .expect_err("payer-auth reservation pushes the 8130 tx over the block gas limit");
    assert!(format!("{err}").contains("more than the block's available gas"), "got: {err}");
}

#[test]
fn exempts_pre_regolith_deposit_from_block_gas_limit() {
    // Pre-Regolith (Bedrock) deposits are exempt from the block-gas check.
    let mut executor = executor_with_block_gas_limit(BaseSpecId::new(BaseUpgrade::Bedrock), 50_000);
    let deposit = TxDeposit {
        from: SENDER,
        to: TxKind::Call(TARGET),
        gas_limit: 100_000,
        ..Default::default()
    };
    executor
        .execute_transaction(&Recovered::new_unchecked(BaseTxEnvelope::Deposit(deposit), SENDER))
        .expect("pre-Regolith deposit is exempt from the block gas limit");
}
