//! Gas-limit fee billing across the Everest boundary: the production payload builder bills a
//! transaction for its declared `gas_limit` from Everest, the block accounts for that gas, and the
//! verifier re-derives the same blocks.

use alloy_consensus::{BlockHeader, Transaction, TxReceipt};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use base_common_consensus::{BaseBlock, BaseReceipt};

use crate::env::{EVEREST_ACTIVATION_TIMESTAMP, EverestTestEnv};

/// Gas limit of the transfers, far above the 21 000 a plain transfer consumes.
const TRANSFER_GAS_LIMIT: u64 = 100_000;
/// Gas a plain value transfer consumes.
const TRANSFER_GAS_USED: u64 = 21_000;

/// Builds the next block with one over-provisioned transfer from the harness test account.
async fn build_block_with_transfer(env: &mut EverestTestEnv) -> BaseBlock {
    let tx = {
        let account = env.sequencer.test_account();
        let mut account = account.lock().expect("test account lock");
        account.create_tx(
            env.chain_id,
            TxKind::Call(Address::ZERO),
            Bytes::new(),
            U256::from(1),
            TRANSFER_GAS_LIMIT,
        )
    };
    env.sequencer.build_next_block_with_transactions(vec![tx]).await
}

/// The gas a block's receipts attribute to each transaction, as `(deposit gas, user gas)`.
fn gas_per_transaction(deposits: &[BaseReceipt], user: &[BaseReceipt]) -> (Vec<u64>, Vec<u64>) {
    let mut previous = 0;
    let mut per_tx = |receipt: &BaseReceipt| {
        let cumulative = receipt.cumulative_gas_used();
        let gas = cumulative - previous;
        previous = cumulative;
        gas
    };
    let deposit_gas = deposits.iter().map(&mut per_tx).collect();
    let user_gas = user.iter().map(&mut per_tx).collect();
    (deposit_gas, user_gas)
}

/// Before Everest a transfer with a 100k gas limit is accounted at the 21k it consumes; from
/// Everest it is accounted at the full 100k, the block's `gas_used` follows, deposits keep their
/// metered gas, and the verifier re-derives both blocks.
#[tokio::test]
async fn everest_bills_transfers_for_their_gas_limit_and_derives() {
    let mut env = EverestTestEnv::new();

    let pre_everest = build_block_with_transfer(&mut env).await;
    assert!(pre_everest.header.timestamp() < EVEREST_ACTIVATION_TIMESTAMP);
    let everest = build_block_with_transfer(&mut env).await;
    assert_eq!(everest.header.timestamp(), EVEREST_ACTIVATION_TIMESTAMP);

    for (block, expected_user_gas) in
        [(&pre_everest, TRANSFER_GAS_USED), (&everest, TRANSFER_GAS_LIMIT)]
    {
        let number = block.header.number;
        let (deposits, user) = env.receipts(block);
        assert!(user.iter().all(TxReceipt::status), "block {number}: transfer must succeed");
        let (deposit_gas, user_gas) = gas_per_transaction(&deposits, &user);
        assert_eq!(
            user_gas,
            vec![expected_user_gas],
            "block {number}: the transfer must be accounted at {expected_user_gas} gas"
        );
        // Deposits are never billed for their limit: every deposit receipt, including the
        // upgrade deposits of the activation block, reports less than its declared gas limit.
        let deposit_limits =
            block.body.transactions.iter().take(deposits.len()).map(Transaction::gas_limit);
        for (index, (gas, limit)) in deposit_gas.iter().zip(deposit_limits).enumerate() {
            assert!(
                *gas < limit,
                "block {number}: deposit {index} must keep its metered gas, got {gas} of {limit}"
            );
        }
        assert_eq!(
            block.header.gas_used(),
            user.last().expect("one user receipt").cumulative_gas_used(),
            "block {number}: header gas_used must equal the receipts' cumulative gas"
        );
    }

    let pre_everest_number = pre_everest.header.number;
    let everest_number = everest.header.number;
    env.derive_blocks(
        [(pre_everest, pre_everest_number), (everest, everest_number)],
        everest_number,
    )
    .await;
}
