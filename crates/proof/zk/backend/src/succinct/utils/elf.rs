//! Embedded SP1 program bytes and proving-key setup.

#[cfg(test)]
use alloy_primitives::b256;
use alloy_primitives::{B256, keccak256};
use anyhow::{Context, Result};
use base_proof_succinct_elfs::{AGGREGATION_ELF, RANGE_ELF_EMBEDDED};
use base_proof_zk_utils::types::u32_to_u8;
use sp1_sdk::{
    Elf, HashableKey, ProvingKey, SP1ProvingKey, SP1VerifyingKey,
    blocking::{CpuProver, LightProver, Prover as BlockingProver},
};

/// Get the range ELF.
pub const fn get_range_elf_embedded() -> &'static [u8] {
    RANGE_ELF_EMBEDDED
}

/// Set up range and aggregation proving/verifying keys via blocking `CpuProver`.
///
/// Runs in `spawn_blocking` because `CpuProver` creates its own tokio runtime
/// internally, which would panic if called directly from an async context.
pub async fn cluster_setup_keys()
-> Result<(SP1ProvingKey, SP1VerifyingKey, SP1ProvingKey, SP1VerifyingKey)> {
    tokio::task::spawn_blocking(|| {
        let cpu_prover = CpuProver::new();
        let range_pk = cpu_prover
            .setup(Elf::Static(get_range_elf_embedded()))
            .context("range ELF setup failed")?;
        let range_vk = range_pk.verifying_key().clone();
        let agg_pk =
            cpu_prover.setup(Elf::Static(AGGREGATION_ELF)).context("agg ELF setup failed")?;
        let agg_vk = agg_pk.verifying_key().clone();
        anyhow::Ok((range_pk, range_vk, agg_pk, agg_vk))
    })
    .await?
}

/// Set up only the range proving key via blocking `CpuProver`.
///
/// Runs in `spawn_blocking` because `CpuProver` creates its own tokio runtime
/// internally, which would panic if called directly from an async context.
pub async fn cluster_setup_range_key() -> Result<SP1ProvingKey> {
    tokio::task::spawn_blocking(|| {
        let cpu_prover = CpuProver::new();
        cpu_prover.setup(Elf::Static(get_range_elf_embedded())).context("range ELF setup failed")
    })
    .await?
}

/// Compute only the verifying keys for the range and aggregation ELFs.
///
/// Uses [`LightProver`] which skips the expensive proving-key generation,
/// making this orders of magnitude faster than [`cluster_setup_keys`].
/// Use this when you only need VKs (e.g. the ZK prover service startup,
/// vkey hash generation).
pub async fn cluster_setup_vkeys() -> Result<(SP1VerifyingKey, SP1VerifyingKey)> {
    tokio::task::spawn_blocking(|| {
        let prover = LightProver::new();
        let range_pk = prover
            .setup(Elf::Static(get_range_elf_embedded()))
            .context("range ELF setup failed")?;
        let range_vk = range_pk.verifying_key().clone();
        let agg_pk = prover.setup(Elf::Static(AGGREGATION_ELF)).context("agg ELF setup failed")?;
        let agg_vk = agg_pk.verifying_key().clone();
        anyhow::Ok((range_vk, agg_vk))
    })
    .await?
}

/// Returns the range verification-key commitment as `AggregateVerifier.ZK_RANGE_HASH`
/// holds it.
///
/// `ZK_RANGE_HASH` is packed into the proof journal that the aggregation program
/// commits to, so it must be the Poseidon2 digest (`hash_u32`) rather than the
/// BN254 digest.
#[must_use]
pub fn range_vkey_commitment(range_vk: &SP1VerifyingKey) -> B256 {
    B256::from(u32_to_u8(range_vk.hash_u32()))
}

/// Returns the aggregation verification key as `AggregateVerifier.ZK_AGGREGATE_HASH`
/// holds it.
///
/// `ZK_AGGREGATE_HASH` is handed to `ZK_VERIFIER.verify` as the SP1 `programVKey`,
/// so it must be the BN254 digest (`bytes32_raw`). This is a *different* value from
/// [`range_vkey_commitment`]'s Poseidon2 digest — using `hash_u32` here yields a hash
/// that never matches the contract and leaves every ZK job unclaimable.
#[must_use]
pub fn aggregate_vkey(aggregate_vk: &SP1VerifyingKey) -> B256 {
    B256::from(aggregate_vk.bytes32_raw())
}

/// Combines the two verification-key commitments into the prover-service routing hash.
///
/// Must stay byte-identical to `ProofArtifacts::zk_artifact_hash` in
/// `base-proof-contracts`, which derives the same value from the on-chain
/// `ZK_RANGE_HASH` and `ZK_AGGREGATE_HASH`.
#[must_use]
fn zk_artifact_hash_from_parts(range_hash: B256, aggregate_hash: B256) -> B256 {
    keccak256([range_hash.as_slice(), aggregate_hash.as_slice()].concat())
}

/// Computes the routing hash for the embedded range and aggregation programs.
pub async fn zk_artifact_hash() -> Result<B256> {
    let (range_vk, aggregate_vk) = cluster_setup_vkeys().await?;
    Ok(zk_artifact_hash_from_parts(range_vkey_commitment(&range_vk), aggregate_vkey(&aggregate_vk)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared vector with `base_proof_contracts::ProofArtifacts::zk_artifact_hash`.
    ///
    /// The worker derives the routing hash from its embedded verification keys and
    /// the challenger derives it from the game's on-chain commitments. If the two
    /// implementations drift, every ZK job silently stays queued, so both crates
    /// pin this exact vector.
    #[test]
    fn zk_artifact_hash_matches_shared_vector() {
        assert_eq!(
            zk_artifact_hash_from_parts(B256::repeat_byte(0x22), B256::repeat_byte(0x33)),
            b256!("0xf3357627f4934d47fe409005b05c900777a6d97ec3788304e2d9c7b4d322cd4d"),
        );
    }
}
