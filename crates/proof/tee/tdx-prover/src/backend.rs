use std::sync::Arc;

use alloy_primitives::B256;
use async_trait::async_trait;
use base_common_chains::ChainConfig;
use base_common_evm::BaseEvmFactory;
use base_common_genesis::RollupConfig;
use base_proof::BootInfo;
use base_proof_client::Prologue;
use base_proof_primitives::{PerChainConfig, ProofJournal, ProofResult, Proposal, ProverBackend};
use base_proof_tee_tdx_runtime::TdxRuntime;

use crate::{Oracle, Result, TdxMeasurements, TdxProverError};

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

    /// Collects a fresh quote and returns its contract-compatible image hash.
    pub fn current_image_hash(&self) -> Result<B256> {
        let quote = self.runtime.signer_quote()?;
        Ok(TdxMeasurements::from_quote(&quote.quote)?.image_hash())
    }
}

#[async_trait]
impl ProverBackend for TdxBackend {
    type Oracle = Oracle;
    type Error = TdxProverError;

    fn create_oracle(&self) -> Oracle {
        Oracle::empty()
    }

    async fn prove(&self, witness: Oracle) -> Result<ProofResult> {
        let tee_image_hash = self.current_image_hash()?;
        let boot_info = BootInfo::load(&witness).await.map_err(pipeline_err)?;
        let cfg = ChainConfig::by_chain_id(boot_info.chain_id)
            .ok_or(TdxProverError::UnsupportedChain(boot_info.chain_id))?;
        let rollup = RollupConfig::from(cfg);
        let config_hash = PerChainConfig::hash_from_rollup_config(&rollup)
            .ok_or(TdxProverError::UnsupportedChain(boot_info.chain_id))?;
        let agreed_l2_output_root = boot_info.agreed_l2_output_root;

        let prologue = Prologue::new(witness.clone(), witness, BaseEvmFactory::default());
        let driver = prologue.load().await.map_err(pipeline_err)?;
        let (epilogue, block_results) =
            driver.execute_with_intermediates().await.map_err(pipeline_err)?;

        if block_results.is_empty() {
            return Err(TdxProverError::ProofPipeline("no proposals produced".into()));
        }

        epilogue.validate().map_err(pipeline_err)?;

        let mut proposals = Vec::with_capacity(block_results.len());
        let mut prev_output_root = agreed_l2_output_root;

        let l1_origin_hash = boot_info.l1_head;
        let l1_origin_number = boot_info.l1_head_number;
        for (l2_info, output_root) in &block_results {
            let l2_block_number = l2_info.block_info.number;
            let journal = ProofJournal {
                proposer: boot_info.proposer,
                l1_origin_hash,
                prev_output_root,
                starting_l2_block: l2_block_number
                    .checked_sub(1)
                    .ok_or_else(|| TdxProverError::ProofPipeline("l2_block_number is 0".into()))?,
                output_root: *output_root,
                ending_l2_block: l2_block_number,
                intermediate_roots: vec![],
                config_hash,
                tee_image_hash,
            };

            proposals.push(Proposal {
                output_root: *output_root,
                signature: self.runtime.sign(journal.encode().as_slice())?,
                l1_origin_hash,
                l1_origin_number,
                l2_block_number,
                prev_output_root,
                config_hash,
            });

            prev_output_root = *output_root;
        }

        let aggregate_proposal = if proposals.len() == 1 {
            proposals[0].clone()
        } else {
            let first = &proposals[0];
            let last = proposals.last().expect("non-empty proposals");

            let interval = boot_info.intermediate_block_interval;
            if interval == 0 {
                return Err(TdxProverError::ProofPipeline(
                    "intermediate_block_interval must not be zero".into(),
                ));
            }
            let interval = interval as usize;
            let intermediate_roots: Vec<B256> = proposals
                .chunks_exact(interval)
                .map(|chunk| chunk[interval - 1].output_root)
                .collect();

            let journal = ProofJournal {
                proposer: boot_info.proposer,
                l1_origin_hash,
                prev_output_root: agreed_l2_output_root,
                starting_l2_block: first
                    .l2_block_number
                    .checked_sub(1)
                    .ok_or_else(|| TdxProverError::ProofPipeline("l2_block_number is 0".into()))?,
                output_root: last.output_root,
                ending_l2_block: last.l2_block_number,
                intermediate_roots,
                config_hash,
                tee_image_hash,
            };

            Proposal {
                output_root: last.output_root,
                signature: self.runtime.sign(journal.encode().as_slice())?,
                l1_origin_hash,
                l1_origin_number,
                l2_block_number: last.l2_block_number,
                prev_output_root: agreed_l2_output_root,
                config_hash,
            }
        };

        Ok(ProofResult::Tee { aggregate_proposal, proposals })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MeasuredMockTdxQuoteProvider;

    fn test_backend() -> TdxBackend {
        let runtime = TdxRuntime::new(MeasuredMockTdxQuoteProvider::local_mock());
        TdxBackend::new(Arc::new(runtime))
    }

    #[test]
    fn current_image_hash_comes_from_current_quote_measurements() {
        let backend = test_backend();

        assert_eq!(
            backend.current_image_hash().unwrap(),
            TdxMeasurements::local_mock().image_hash()
        );
    }
}
