//! EIP-8130 transaction tests across the Base Cobalt boundary.
use alloy_consensus::TxReceipt;
use base_common_consensus::{BaseReceipt, BaseTxEnvelope};

use crate::env::CobaltTestEnv;

#[tokio::test]
async fn eip8130_transaction_executes_and_derives_at_cobalt() {
    let mut env = CobaltTestEnv::new();

    // Block 1 has timestamp 2, before Cobalt.
    let pre_cobalt = env.sequencer.build_empty_block().await;

    // Block 2 has timestamp 4, exactly when Cobalt activates.
    let tx = env.create_eip8130_tx(0);
    let cobalt_block = env.sequencer.build_next_block_with_transactions(vec![tx]).await;

    // Inspect the transaction receipt and verify it succeeded.
    // The receipt should be BaseReceipt::Eip8130.

    let receipt = env.user_tx_receipt(&cobalt_block, 0);

    let BaseReceipt::Eip8130(receipt) = receipt else {
        panic!("expected an EIP-8130 receipt");
    };

    assert!(receipt.inner.status());
    assert_eq!(receipt.phase_statuses, vec![0x01]);

    // Send both blocks through the batcher and verifier.
    env.derive_blocks([(pre_cobalt, 1), (cobalt_block, 2)], 2, "Cobalt").await;
}

#[tokio::test]
async fn eip8130_transaction_is_rejected_before_cobalt() {
    let mut env = CobaltTestEnv::new();

    // The first block the sequencer builds has timestamp 2, before Cobalt
    // activates at timestamp 4. The enshrined execution path rejects the
    // EIP-8130 transaction type before activation, so it must not be included
    // in the block.
    let tx = env.create_eip8130_tx(0);
    let block = env.sequencer.build_next_block_with_transactions(vec![tx]).await;

    let user_tx_count = block
        .body
        .transactions
        .iter()
        .filter(|tx| !matches!(tx, BaseTxEnvelope::Deposit(_)))
        .count();
    assert_eq!(user_tx_count, 0, "EIP-8130 transaction must be excluded before Cobalt");
}
