use crate::{
    args::OpRbuilderArgs,
    tests::{ChainDriverExt, LocalInstance},
};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, TxHash};
use macros::rb_test;

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// Test that the uncompressed (EIP-2718 encoded) block size limit correctly
/// prevents oversized blocks. We set a 5000-byte limit and send 4 transactions
/// each ~2100 bytes encoded (2000 bytes of calldata). Only ~2 fit per block,
/// so the remaining should spill to the next block.
#[rb_test(args = OpRbuilderArgs {
    max_uncompressed_block_size: Some(5000),
    ..Default::default()
})]
async fn uncompressed_size_limit_splits_across_blocks(rbuilder: LocalInstance) -> eyre::Result<()> {
    let driver = rbuilder.driver().await?;

    // Fund 4 separate accounts so each tx is independent (no nonce dependencies)
    let accounts = driver.fund_accounts(4, ONE_ETH).await?;

    // Each tx has 2000 bytes of calldata, making the EIP-2718 encoded size ~2100 bytes.
    // With a 5000-byte limit, ~2 txs fit per block (2*2100=4200 < 5000, 3*2100=6300 > 5000).
    let calldata = Bytes::from(vec![0x42u8; 2000]);
    let mut all_tx_hashes = Vec::new();

    for account in &accounts {
        let tx = driver
            .create_transaction()
            .with_signer(account.clone())
            .with_to(rand::random::<Address>())
            .with_value(1)
            .with_input(calldata.clone())
            .send()
            .await?;
        all_tx_hashes.push(*tx.tx_hash());
    }

    // Block 1: should include some txs but not all
    let block1 = driver.build_new_block_with_current_timestamp(None).await?;
    let block1_included: Vec<TxHash> = all_tx_hashes
        .iter()
        .filter(|h| block1.transactions.hashes().any(|bh| bh == **h))
        .copied()
        .collect();

    assert!(
        !block1_included.is_empty(),
        "Block 1 should include at least one user transaction"
    );
    assert!(
        block1_included.len() < all_tx_hashes.len(),
        "Block 1 should NOT include all {} transactions due to uncompressed size limit (included {})",
        all_tx_hashes.len(),
        block1_included.len()
    );

    let block1_uncompressed_size: u64 = block1
        .transactions
        .txns()
        .map(|tx| tx.inner.inner.encode_2718_len() as u64)
        .sum();
    assert!(
        block1_uncompressed_size <= 5000,
        "Block 1 uncompressed size ({block1_uncompressed_size}) should not exceed 5000 bytes"
    );

    // Block 2: should include the remaining transactions
    let block2 = driver.build_new_block_with_current_timestamp(None).await?;
    let block2_included: Vec<TxHash> = all_tx_hashes
        .iter()
        .filter(|h| block2.transactions.hashes().any(|bh| bh == **h))
        .copied()
        .collect();

    assert!(
        !block2_included.is_empty(),
        "Block 2 should include remaining transactions"
    );

    let block2_uncompressed_size: u64 = block2
        .transactions
        .txns()
        .map(|tx| tx.inner.inner.encode_2718_len() as u64)
        .sum();
    assert!(
        block2_uncompressed_size <= 5000,
        "Block 2 uncompressed size ({block2_uncompressed_size}) should not exceed 5000 bytes"
    );

    // Verify all transactions are included across both blocks
    let total = block1_included.len() + block2_included.len();
    assert_eq!(
        total,
        all_tx_hashes.len(),
        "All {} transactions should be included across 2 blocks (got {})",
        all_tx_hashes.len(),
        total
    );

    Ok(())
}
