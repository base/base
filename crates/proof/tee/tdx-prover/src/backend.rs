use std::{
    fmt,
    sync::{Arc, LazyLock},
};

use alloy_primitives::{B256, Bytes, map::HashMap};
use async_trait::async_trait;
use base_common_chains::ChainConfig;
use base_common_evm::BaseEvmFactory;
use base_common_genesis::RollupConfig;
use base_proof::BootInfo;
use base_proof_client::Prologue;
use base_proof_primitives::{PerChainConfig, ProofJournal, ProofResult, Proposal, ProverBackend};
use base_proof_tee_tdx_runtime::{TdxQuoteProvider, TdxRuntime};

use crate::{Oracle, Result, TdxMeasurements, TdxProverError};

/// Inputs needed to build a signed aggregate proposal.
#[derive(Debug)]
pub struct AggregateProposalInput<'a> {
    /// Boot info that supplied proposer, L1 origin, and interval values.
    pub boot_info: &'a BootInfo,
    /// Per-block proposals being aggregated.
    pub proposals: &'a [Proposal],
    /// Agreed output root preceding the aggregate range.
    pub agreed_l2_output_root: B256,
    /// Per-chain config hash.
    pub config_hash: B256,
    /// TDX image hash used in the signed proof journal.
    pub tee_image_hash: B256,
}

const NO_PROPOSALS_ERR: &str = "no proposals produced";
const ZERO_L2_BLOCK_ERR: &str = "l2_block_number is 0";

fn pipeline_err(err: impl ToString) -> TdxProverError {
    TdxProverError::ProofPipeline(err.to_string())
}

/// Per-chain config hashes derived from [`ChainConfig::all`] at first access.
pub static CONFIG_HASHES: LazyLock<HashMap<u64, B256>> = LazyLock::new(|| {
    let mut map = HashMap::default();
    for cfg in ChainConfig::all() {
        let rollup = RollupConfig::from(cfg);
        if let Some(mut per_chain) = PerChainConfig::from_rollup_config(&rollup) {
            per_chain.force_defaults();
            map.insert(cfg.chain_id, per_chain.hash());
        }
    }
    map
});

/// TEE proof backend that executes the proof pipeline with a TDX signer.
pub struct TdxBackend<P> {
    runtime: Arc<TdxRuntime<P>>,
}

impl<P> TdxBackend<P> {
    /// Create a new backend using the given TDX runtime.
    pub const fn new(runtime: Arc<TdxRuntime<P>>) -> Self {
        Self { runtime }
    }

    /// Returns the TDX runtime used by this backend.
    pub const fn runtime(&self) -> &Arc<TdxRuntime<P>> {
        &self.runtime
    }

    /// Signs the exact `ProofJournal` bytes expected by the onchain TEE verifier.
    pub fn sign_proof_journal(&self, journal: &ProofJournal) -> Result<Bytes> {
        self.runtime.sign(journal.encode().as_slice()).map_err(Into::into)
    }

    /// Look up the config hash for a supported chain.
    pub fn config_hash_for_chain(chain_id: u64) -> Result<B256> {
        CONFIG_HASHES.get(&chain_id).copied().ok_or(TdxProverError::UnsupportedChain(chain_id))
    }
}

impl<P: TdxQuoteProvider> TdxBackend<P> {
    /// Collects a fresh quote and returns its contract-compatible image hash.
    pub fn current_image_hash(&self) -> Result<B256> {
        let quote = self.runtime.signer_quote()?;
        Ok(TdxMeasurements::from_quote(&quote.quote)?.image_hash())
    }

    /// Runs the proof-client pipeline over preimages with an explicit TDX image hash.
    pub async fn prove_with_image_hash(
        &self,
        preimages: impl IntoIterator<Item = (base_proof_preimage::PreimageKey, Vec<u8>)>,
        tee_image_hash: B256,
    ) -> Result<ProofResult> {
        let oracle = Oracle::new(preimages)?;
        let boot_info = BootInfo::load(&oracle).await.map_err(pipeline_err)?;
        let config_hash = Self::config_hash_for_chain(boot_info.chain_id)?;
        let agreed_l2_output_root = boot_info.agreed_l2_output_root;

        let prologue = Prologue::new(oracle.clone(), oracle, BaseEvmFactory::default());
        let driver = prologue.load().await.map_err(pipeline_err)?;
        let (epilogue, block_results) =
            driver.execute_with_intermediates().await.map_err(pipeline_err)?;

        if block_results.is_empty() {
            return Err(TdxProverError::ProofPipeline(NO_PROPOSALS_ERR.into()));
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
                    .ok_or_else(|| TdxProverError::ProofPipeline(ZERO_L2_BLOCK_ERR.into()))?,
                output_root: *output_root,
                ending_l2_block: l2_block_number,
                intermediate_roots: vec![],
                config_hash,
                tee_image_hash,
            };

            proposals.push(Proposal {
                output_root: *output_root,
                signature: self.sign_proof_journal(&journal)?,
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
            self.aggregate_proposal(AggregateProposalInput {
                boot_info: &boot_info,
                proposals: &proposals,
                agreed_l2_output_root,
                config_hash,
                tee_image_hash,
            })?
        };

        Ok(ProofResult::Tee { aggregate_proposal, proposals })
    }

    /// Builds and signs an aggregate proposal for a multi-block proof result.
    pub fn aggregate_proposal(&self, input: AggregateProposalInput<'_>) -> Result<Proposal> {
        let first = input
            .proposals
            .first()
            .ok_or_else(|| TdxProverError::ProofPipeline(NO_PROPOSALS_ERR.into()))?;
        let last = input
            .proposals
            .last()
            .ok_or_else(|| TdxProverError::ProofPipeline(NO_PROPOSALS_ERR.into()))?;

        let interval = input.boot_info.intermediate_block_interval;
        if interval == 0 {
            return Err(TdxProverError::ProofPipeline(
                "intermediate_block_interval must not be zero".into(),
            ));
        }
        let interval = interval as usize;
        let count = input.proposals.len() / interval;
        let intermediate_roots: Vec<B256> =
            (1..=count).map(|i| input.proposals[i * interval - 1].output_root).collect();

        let l1_origin_hash = input.boot_info.l1_head;
        let l1_origin_number = input.boot_info.l1_head_number;

        let journal = ProofJournal {
            proposer: input.boot_info.proposer,
            l1_origin_hash,
            prev_output_root: input.agreed_l2_output_root,
            starting_l2_block: first
                .l2_block_number
                .checked_sub(1)
                .ok_or_else(|| TdxProverError::ProofPipeline(ZERO_L2_BLOCK_ERR.into()))?,
            output_root: last.output_root,
            ending_l2_block: last.l2_block_number,
            intermediate_roots,
            config_hash: input.config_hash,
            tee_image_hash: input.tee_image_hash,
        };

        Ok(Proposal {
            output_root: last.output_root,
            signature: self.sign_proof_journal(&journal)?,
            l1_origin_hash,
            l1_origin_number,
            l2_block_number: last.l2_block_number,
            prev_output_root: input.agreed_l2_output_root,
            config_hash: input.config_hash,
        })
    }
}

impl<P> fmt::Debug for TdxBackend<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TdxBackend").finish_non_exhaustive()
    }
}

#[async_trait]
impl<P> ProverBackend for TdxBackend<P>
where
    P: TdxQuoteProvider + fmt::Debug + 'static,
{
    type Oracle = Oracle;
    type Error = TdxProverError;

    fn create_oracle(&self) -> Oracle {
        Oracle::empty()
    }

    async fn prove(&self, witness: Oracle) -> Result<ProofResult> {
        let image_hash = self.current_image_hash()?;
        let preimages = witness.into_preimages()?;
        self.prove_with_image_hash(preimages, image_hash).await
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use base_proof_primitives::ProofJournal;
    use k256::ecdsa::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier};

    use super::*;
    use crate::MeasuredMockTdxQuoteProvider;

    fn test_backend() -> TdxBackend<MeasuredMockTdxQuoteProvider> {
        let runtime = TdxRuntime::new(MeasuredMockTdxQuoteProvider::local_mock());
        TdxBackend::new(Arc::new(runtime))
    }

    fn test_journal() -> ProofJournal {
        ProofJournal {
            proposer: address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            l1_origin_hash: b256!(
                "2222222222222222222222222222222222222222222222222222222222222222"
            ),
            prev_output_root: b256!(
                "3333333333333333333333333333333333333333333333333333333333333333"
            ),
            starting_l2_block: 999,
            output_root: b256!("4444444444444444444444444444444444444444444444444444444444444444"),
            ending_l2_block: 1000,
            intermediate_roots: vec![],
            config_hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            tee_image_hash: b256!(
                "5555555555555555555555555555555555555555555555555555555555555555"
            ),
        }
    }

    #[test]
    fn current_image_hash_comes_from_current_quote_measurements() {
        let backend = test_backend();

        assert_eq!(
            backend.current_image_hash().unwrap(),
            TdxMeasurements::local_mock().image_hash()
        );
    }

    #[test]
    fn tdx_server_signs_tee_verifier_proof_journal_bytes() {
        let backend = test_backend();
        let journal = test_journal();
        let signature = backend.sign_proof_journal(&journal).unwrap();

        let public_key = backend.runtime().signer_public_key();
        let verifying_key = VerifyingKey::from_sec1_bytes(&public_key).unwrap();
        let signature = Signature::from_slice(&signature[..64]).unwrap();
        let hash = alloy_primitives::keccak256(journal.encode());

        assert!(verifying_key.verify_prehash(hash.as_slice(), &signature).is_ok());
    }
}
