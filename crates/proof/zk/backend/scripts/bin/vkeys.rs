//! Binary for generating and displaying Succinct verification key hashes.

use anyhow::Result;
use base_proof_zk_backend::{aggregate_vkey, cluster_setup_vkeys, range_vkey_commitment};

#[tokio::main]
async fn main() -> Result<()> {
    let (range_vk, agg_vk) = cluster_setup_vkeys().await?;

    // These use different SP1 digests on purpose: the range commitment is the
    // Poseidon2 digest packed into the proof journal, the aggregation key is the
    // BN254 digest the on-chain SP1 verifier takes as its `programVKey`.
    println!("Range Verification Key Hash: {}", range_vkey_commitment(&range_vk));
    println!("Aggregation Verification Key Hash: {}", aggregate_vkey(&agg_vk));

    Ok(())
}
