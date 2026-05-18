use std::{fmt::Debug, sync::Arc};

use alloy_genesis::ChainConfig;
use alloy_primitives::Sealed;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_consensus_derive::{
    BlobProvider, ChainProvider, DataAvailabilityProvider, L2ChainProvider, Pipeline,
    SignalReceiver,
};
use base_proof::{
    BaseExecutor, BootInfo, OracleL1ChainProvider, OracleL2ChainProvider, OraclePipeline,
    new_oracle_pipeline_cursor,
};
use base_proof_driver::{Driver, DriverPipeline, PipelineCursor};
use base_proof_executor::TrieDBProvider;
use base_proof_preimage::{CommsClient, FlushableCache};
use spin::RwLock;
use tracing::info;

use crate::{
    client::{advance_to_target, fetch_safe_head_hash},
    precompiles::{CustomCrypto, ZkvmBaseEvmFactory},
};

// Gets the inputs for constructing the derivation pipeline.
/// Returns (`BootInfo`, pipeline components, `safe_head_block_number`).
/// The safe head block number is the L2 block number of the agreed-upon safe head,
/// i.e. the starting block of the proven range.
pub async fn get_inputs_for_pipeline<O>(
    oracle: Arc<O>,
) -> Result<(
    BootInfo,
    (Arc<RwLock<PipelineCursor>>, OracleL1ChainProvider<O>, OracleL2ChainProvider<O>),
    u64,
)>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
{
    ////////////////////////////////////////////////////////////////
    //                          PROLOGUE                          //
    ////////////////////////////////////////////////////////////////

    let boot = match BootInfo::load(oracle.as_ref()).await {
        Ok(boot) => boot,
        Err(e) => {
            return Err(anyhow!("Failed to load boot info: {e:?}"));
        }
    };

    let boot_clone = boot.clone();

    let rollup_config = Arc::new(boot.rollup_config);
    let safe_head_hash = fetch_safe_head_hash(oracle.as_ref(), boot.agreed_l2_output_root).await?;

    let mut l1_provider = OracleL1ChainProvider::new(boot.l1_head, Arc::clone(&oracle));
    let mut l2_provider =
        OracleL2ChainProvider::new(safe_head_hash, Arc::clone(&rollup_config), Arc::clone(&oracle));

    // Fetch the safe head's block header.
    let safe_head = l2_provider
        .header_by_hash(safe_head_hash)
        .map(|header| Sealed::new_unchecked(header, safe_head_hash))?;

    let safe_head_number = safe_head.number;

    // If the claimed L2 block number is less than the safe head of the L2 chain, the claim is
    // invalid.
    if boot.claimed_l2_block_number < safe_head.number {
        return Err(anyhow!(
            "Claimed L2 block number {claimed} is less than the safe head {safe}",
            claimed = boot.claimed_l2_block_number,
            safe = safe_head.number
        ));
    }

    ////////////////////////////////////////////////////////////////
    //                   DERIVATION & EXECUTION                   //
    ////////////////////////////////////////////////////////////////

    // Create a new derivation driver with the given boot information and oracle.
    let cursor = new_oracle_pipeline_cursor(
        rollup_config.as_ref(),
        safe_head,
        boot.agreed_l2_output_root,
        &mut l1_provider,
        &mut l2_provider,
    )
    .await?;
    l2_provider.set_cursor(Arc::clone(&cursor));

    Ok((boot_clone, (cursor, l1_provider, l2_provider), safe_head_number))
}

/// Constructs a derivation pipeline and executes block derivation.
#[async_trait]
pub trait WitnessExecutor {
    /// Oracle client.
    type O: CommsClient + FlushableCache + Send + Sync + Debug;
    /// Blob provider.
    type B: BlobProvider + Send + Sync + Debug + Clone;
    /// L1 chain data provider.
    type L1: ChainProvider + Send + Sync + Debug + Clone;
    /// L2 chain data provider.
    type L2: L2ChainProvider + Send + Sync + Debug + Clone;
    /// Data availability provider.
    type DA: DataAvailabilityProvider + Send + Sync + Debug + Clone;

    /// Build the derivation pipeline from the given providers.
    #[allow(clippy::too_many_arguments)]
    async fn create_pipeline(
        &self,
        rollup_config: Arc<RollupConfig>,
        l1_config: Arc<ChainConfig>,
        cursor: Arc<RwLock<PipelineCursor>>,
        oracle: Arc<Self::O>,
        beacon: Self::B,
        l1_provider: Self::L1,
        l2_provider: Self::L2,
    ) -> Result<OraclePipeline<Self::O, Self::L1, Self::L2, Self::DA>>;

    /// Run derivation and block execution to produce the proven boot info and derived L2 block.
    ///
    /// The intermediate root sampling interval is taken from
    /// [`BootInfo::intermediate_block_interval`] (committed via preimage key 9, same source as
    /// the TEE), so callers do not need to pass it explicitly.
    async fn run<O, DP, P>(
        &self,
        boot: BootInfo,
        pipeline: DP,
        cursor: Arc<RwLock<PipelineCursor>>,
        l2_provider: OracleL2ChainProvider<O>,
    ) -> Result<(BootInfo, u64, Vec<alloy_primitives::B256>)>
    where
        O: CommsClient + FlushableCache + Send + Sync + Debug,
        DP: DriverPipeline<P> + Send + Sync + Debug,
        P: Pipeline + SignalReceiver + Send + Sync + Debug,
    {
        // Install custom crypto provider for KZG point evaluation precompile
        revm::precompile::install_crypto(CustomCrypto::default());

        let boot_clone = boot.clone();
        let intermediate_block_interval = boot.intermediate_block_interval.max(1);

        let rollup_config = Arc::new(boot.rollup_config);

        let executor = BaseExecutor::new(
            rollup_config.as_ref(),
            l2_provider.clone(),
            l2_provider,
            ZkvmBaseEvmFactory::new(),
            None,
        );
        let mut driver = Driver::new(cursor, executor, pipeline);
        // Run the derivation pipeline until we are able to produce the output root of the claimed
        // L2 block.

        // Use custom advance to target with cycle tracking.
        #[cfg(target_os = "zkvm")]
        println!("cycle-tracker-report-start: block-execution-and-derivation");
        let (safe_head, output_root, intermediate_roots) = advance_to_target(
            &mut driver,
            rollup_config.as_ref(),
            Some(boot.claimed_l2_block_number),
            intermediate_block_interval,
        )
        .await?;
        #[cfg(target_os = "zkvm")]
        println!("cycle-tracker-report-end: block-execution-and-derivation");

        ////////////////////////////////////////////////////////////////
        //                          EPILOGUE                          //
        ////////////////////////////////////////////////////////////////

        let derived_l2_block_number = safe_head.block_info.number;
        ensure_derived_block_matches_claim(derived_l2_block_number, boot.claimed_l2_block_number)?;

        if output_root != boot.claimed_l2_output_root {
            return Err(anyhow!(
                "Failed to validate L2 block #{number} with claimed output root {claimed_output_root}. Got {output_root} instead",
                number = derived_l2_block_number,
                output_root = output_root,
                claimed_output_root = boot.claimed_l2_output_root,
            ));
        }

        info!(
            target: "client",
            block_number = derived_l2_block_number,
            output_root = %output_root,
            "successfully validated L2 block"
        );

        // SAFETY: Inside the zkVM, the process exits after proof generation completes.
        // Dropping `driver` and `rollup_config` triggers deep recursive destructors that
        // waste significant zkVM cycles without benefit, since all memory is reclaimed
        // on process exit. We intentionally leak them to avoid this overhead.
        #[cfg(target_os = "zkvm")]
        {
            std::mem::forget(driver);
            std::mem::forget(rollup_config);
        }

        Ok((boot_clone, derived_l2_block_number, intermediate_roots))
    }
}

/// Ensures the derived L2 safe-head block number matches the boot's claimed L2 block number.
pub fn ensure_derived_block_matches_claim(
    safe_head_number: u64,
    claimed_block_number: u64,
) -> Result<()> {
    if safe_head_number != claimed_block_number {
        return Err(anyhow!(
            "Derived safe head L2 block #{safe_head_number} does not match claimed L2 block number #{claimed_block_number}",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ensure_derived_block_matches_claim;

    #[test]
    fn returns_ok_when_derived_equals_claimed() {
        assert!(ensure_derived_block_matches_claim(0, 0).is_ok());
        assert!(ensure_derived_block_matches_claim(123_456, 123_456).is_ok());
        assert!(ensure_derived_block_matches_claim(u64::MAX, u64::MAX).is_ok());
    }

    #[test]
    fn returns_err_with_both_numbers_when_derived_below_claimed() {
        let err = ensure_derived_block_matches_claim(50, 100).expect_err("expected mismatch error");
        let msg = err.to_string();
        assert!(msg.contains("#50"), "missing derived block number in: {msg}");
        assert!(msg.contains("#100"), "missing claimed block number in: {msg}");
        assert!(
            msg.contains("Derived safe head") && msg.contains("claimed L2 block number"),
            "missing expected labels in: {msg}",
        );
    }

    #[test]
    fn returns_err_with_both_numbers_when_derived_above_claimed() {
        let err =
            ensure_derived_block_matches_claim(150, 100).expect_err("expected mismatch error");
        let msg = err.to_string();
        assert!(msg.contains("#150"));
        assert!(msg.contains("#100"));
    }
}
