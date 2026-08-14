//! Embedded SP1 program bytes and proving-key setup.

use anyhow::{Context, Result};
use base_proof_succinct_elfs::{AGGREGATION_ELF, RANGE_ELF_EMBEDDED};
use sp1_sdk::{
    Elf, ProvingKey, SP1ProvingKey, SP1VerifyingKey,
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
