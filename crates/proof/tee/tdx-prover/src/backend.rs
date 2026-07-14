use std::sync::Arc;

use async_trait::async_trait;
use base_common_chains::ChainConfig;
use base_common_evm::BaseEvmFactory;
use base_proof_client::{Prologue, TeeProposals};
use base_proof_primitives::{PerChainConfig, ProofResult, ProverBackend};
use base_proof_tee_tdx_runtime::TdxRuntime;
use base_proof_tee_tdx_verifier::TdxQuote;

use crate::{Oracle, Result, TdxProverError};

fn pipeline_err(err: impl ToString) -> TdxProverError {
    TdxProverError::ProofPipeline(err.to_string())
}

/// TEE proof backend that executes the proof pipeline with a TDX signer.
#[derive(Debug)]
pub struct TdxBackend {
    runtime: Arc<TdxRuntime>,
}

impl TdxBackend {
    /// Create a new backend using the given TDX runtime.
    pub const fn new(runtime: Arc<TdxRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl ProverBackend for TdxBackend {
    type Oracle = Oracle;
    type Error = TdxProverError;

    fn create_oracle(&self) -> Oracle {
        Oracle::default()
    }

    async fn prove(&self, witness: Oracle) -> Result<ProofResult> {
        let prologue = Prologue::new(witness.clone(), witness, BaseEvmFactory::default());
        let (boot_info, driver) = prologue.load().await.map_err(pipeline_err)?;
        let config_hash = ChainConfig::rollup_config_by_chain_id(boot_info.chain_id)
            .and_then(|cfg| {
                let mut per_chain = PerChainConfig::from_rollup_config(&cfg)?;
                per_chain.force_defaults();
                Some(per_chain.hash())
            })
            .ok_or(TdxProverError::UnsupportedChain(boot_info.chain_id))?;
        let quote = TdxQuote::parse(&self.runtime.signer_quote(None)?.quote)?;
        let tee_image_hash = quote.image_hash();
        let (epilogue, block_results) =
            driver.execute_with_intermediates().await.map_err(pipeline_err)?;

        epilogue.validate().map_err(pipeline_err)?;

        TeeProposals::build(
            &boot_info,
            &block_results,
            config_hash,
            tee_image_hash,
            |data| self.runtime.sign(data).map_err(TdxProverError::from),
            pipeline_err,
        )
    }
}
