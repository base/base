//! Binary for generating and displaying Succinct verification key hashes.

use alloy_primitives::B256;
use anyhow::Result;
use base_proof_zk_backend::cluster_setup_vkeys;
use sp1_sdk::HashableKey;

#[tokio::main]
async fn main() -> Result<()> {
    let (range_vk, agg_vk) = cluster_setup_vkeys().await?;

    let range_vk_hash = B256::from(range_vk.hash_bytes());
    println!("Range Verification Key Hash: {range_vk_hash}");
    println!("Aggregation Verification Key Hash: {}", agg_vk.bytes32());

    Ok(())
}
