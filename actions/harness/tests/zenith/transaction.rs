//! EIP-8130 transaction tests across the Base Zenith boundary.
use alloy_consensus::TxReceipt;
use base_common_consensus::BaseReceipt;

use crate::env::ZenithTestEnv;

#[tokio::test]
async fn eip8130_transaction_executes_and_derives_at_zenith() {
    let mut env = ZenithTestEnv::new();

    // Block 1 has timestamp 2, before Zenith.
    let pre_zenith = env.sequencer.build_empty_block().await;

    // Block 2 has timestamp 4, exactly when Zenith activates.
    let tx = env.create_eip8130_tx(0);
    let zenith_block = env.sequencer.build_next_block_with_transactions(vec![tx]).await;

    // The EIP-8130 transaction must produce a successful account-abstraction
    // receipt with a single committed call phase.
    let receipt = env.user_tx_receipt(&zenith_block, 0);
    let BaseReceipt::Eip8130(receipt) = receipt else {
        panic!("expected an EIP-8130 receipt");
    };
    assert!(receipt.inner.status());
    assert_eq!(receipt.phase_statuses, vec![0x01]);

    // Send both blocks through the batcher and verifier, asserting the verifier
    // re-derives them with matching state.
    env.derive_blocks([(pre_zenith, 1), (zenith_block, 2)], 2).await;
}
