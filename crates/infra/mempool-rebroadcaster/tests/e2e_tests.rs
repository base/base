//! End-to-end tests for the mempool rebroadcaster.

use std::path::Path;

use alloy_rpc_types::txpool::TxpoolContent;
use mempool_rebroadcaster::{Rebroadcaster, RebroadcasterConfig};

fn load_static_mempool_content<P: AsRef<Path>>(
    filepath: P,
) -> Result<TxpoolContent, Box<dyn std::error::Error>> {
    let data = std::fs::read_to_string(filepath)?;
    let content: TxpoolContent = serde_json::from_str(&data)?;
    Ok(content)
}

#[tokio::test]
async fn test_e2e_static_data() {
    // Load static test data
    let geth_mempool = load_static_mempool_content("testdata/geth_mempool.json")
        .expect("Failed to load geth mempool data");

    let reth_mempool = load_static_mempool_content("testdata/reth_mempool.json")
        .expect("Failed to load reth mempool data");

    // Use constant network fees for testing (same as Go version)
    let base_fee = 0x2601ff_u128; // 0x2601ff
    let gas_price = 0x36daa7_u128; // 0x36daa7

    // Create a rebroadcaster instance for testing (endpoints don't matter for this test)
    let rebroadcaster = Rebroadcaster::new(RebroadcasterConfig {
        geth_mempool_endpoint: "http://localhost:8545".to_string(),
        reth_mempool_endpoint: "http://localhost:8546".to_string(),
    });

    // Apply filtering logic (same as production)
    let filtered_geth_mempool =
        rebroadcaster.filter_underpriced_txns(&geth_mempool, base_fee, gas_price);
    let filtered_reth_mempool =
        rebroadcaster.filter_underpriced_txns(&reth_mempool, base_fee, gas_price);

    // Compute diff (same as production)
    let diff = rebroadcaster.compute_diff(&filtered_geth_mempool, &filtered_reth_mempool);

    // Expected results: Since reth mempool is empty and geth has 5 transactions,
    // all 5 should be in in_geth_not_in_reth (sorted by nonce)
    let expected_missing_hashes = [
        "0x2d4bce6f850ef7ef164585400506893595460fac2fa8f5631de42667896a8b9a", // nonce 97226
        "0x2ddc43a753e327f21b8feab2f83db4a9add29b353519f7c28c217aa677b7671f", // nonce 97227
        "0x30e74220b9769c0eb68f95908d7f9370728a9369e6fa67e734546254c8ebe5ef", // nonce 97228
        "0x872447406779378c9650847a63ffcc2e29e870aa38f2c9fdb30834212d78f860", // nonce 97229
        "0x4c8a6e278cc52d2b8a53e69e372918fe6980f2d27caa9595ac2434ba0b28cbec", // nonce 97230
    ];

    // Assert transaction count
    assert_eq!(
        expected_missing_hashes.len(),
        diff.in_geth_not_in_reth.len(),
        "in_geth_not_in_reth count should match expected"
    );

    // Assert no transactions in reth but not in geth (since reth is empty)
    assert_eq!(
        0,
        diff.in_reth_not_in_geth.len(),
        "in_reth_not_in_geth should be empty since reth mempool is empty"
    );

    // Assert transactions are sorted by nonce and match expected values
    for (i, tx) in diff.in_geth_not_in_reth.iter().enumerate() {
        let tx_hash = format!("{:#x}", tx.as_recovered().hash());
        assert_eq!(
            expected_missing_hashes[i], tx_hash,
            "Transaction {i} hash should match expected"
        );
    }
}

#[tokio::test]
async fn test_e2e_filtering_logic() {
    // Test that underpriced transactions are properly filtered
    // Load the same data but with higher base fees to test filtering
    let geth_mempool = load_static_mempool_content("testdata/geth_mempool.json")
        .expect("Failed to load geth mempool data");

    // Set very high base fee that should filter out our transactions
    let very_high_base_fee = u128::MAX;
    let very_high_gas_price = u128::MAX;

    // Create a rebroadcaster instance for testing
    let rebroadcaster = Rebroadcaster::new(RebroadcasterConfig {
        geth_mempool_endpoint: "http://localhost:8545".to_string(),
        reth_mempool_endpoint: "http://localhost:8546".to_string(),
    });

    // Apply filtering with very high fees
    let filtered_geth_mempool = rebroadcaster.filter_underpriced_txns(
        &geth_mempool,
        very_high_base_fee,
        very_high_gas_price,
    );

    // Assert all transactions are filtered out due to high base fee
    let total_pending =
        filtered_geth_mempool.pending.values().map(|nonce_txs| nonce_txs.len()).sum::<usize>();
    let total_queued =
        filtered_geth_mempool.queued.values().map(|nonce_txs| nonce_txs.len()).sum::<usize>();

    assert_eq!(
        0,
        total_pending + total_queued,
        "All transactions should be filtered out with very high base fee"
    );
}
