//! Embedded SP1 program bytes and proving-key setup.

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

/// Computes the routing hash for the embedded range and aggregation programs.
pub async fn zk_artifact_hash() -> Result<B256> {
    let (range_vk, aggregate_vk) = cluster_setup_vkeys().await?;
    let range_hash = B256::from(u32_to_u8(range_vk.hash_u32()));
    let aggregate_hash = B256::from(u32_to_u8(aggregate_vk.hash_u32()));
    let mut artifacts = [0_u8; 64];
    artifacts[..32].copy_from_slice(range_hash.as_slice());
    artifacts[32..].copy_from_slice(aggregate_hash.as_slice());
    Ok(keccak256(artifacts))
}
