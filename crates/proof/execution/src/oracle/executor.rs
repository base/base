//! An executor constructor.

use alloc::boxed::Box;
use core::fmt::Debug;

use alloy_primitives::B256;
use async_trait::async_trait;
use base_common_chain_config::RollupConfig;
use base_common_types_chain::{Header, Sealed};
use base_common_types_payload::BasePayloadAttributes;
use base_proof_witness_mpt::TrieHinter;

use crate::{BlockBuildingOutcome, Executor, StatelessL2Builder, TrieDBProvider};

/// An executor wrapper type.
#[derive(Debug)]
pub struct BaseExecutor<'a, P, H>
where
    P: TrieDBProvider + Send + Sync + Clone,
    H: TrieHinter + Send + Sync + Clone,
{
    /// The rollup config for the executor.
    rollup_config: &'a RollupConfig,
    /// The trie provider for the executor.
    trie_provider: P,
    /// The trie hinter for the executor.
    trie_hinter: H,
    /// The evm factory for the executor.
    evm_factory: base_execution_evm_runtime::BaseEvmFactory,
    /// The executor.
    inner: Option<StatelessL2Builder<'a, P, H>>,
}

impl<'a, P, H> BaseExecutor<'a, P, H>
where
    P: TrieDBProvider + Send + Sync + Clone,
    H: TrieHinter + Send + Sync + Clone,
{
    /// Creates a new executor.
    pub const fn new(
        rollup_config: &'a RollupConfig,
        trie_provider: P,
        trie_hinter: H,
        evm_factory: base_execution_evm_runtime::BaseEvmFactory,
        inner: Option<StatelessL2Builder<'a, P, H>>,
    ) -> Self {
        Self { rollup_config, trie_provider, trie_hinter, evm_factory, inner }
    }
}

#[async_trait]
impl<P, H> Executor for BaseExecutor<'_, P, H>
where
    P: TrieDBProvider + Debug + Send + Sync + Clone,
    H: TrieHinter + Debug + Send + Sync + Clone,
{
    type Error = crate::ExecutorError;

    fn is_deposit_only_retryable(error: &Self::Error) -> bool {
        error.is_deposit_only_retryable()
    }

    /// Waits for the executor to be ready.
    async fn wait_until_ready(&mut self) {
        /* no-op for the stateless executor */
        /* This is used when an engine api is used instead of a stateless block executor */
    }

    /// Updates the safe header.
    ///
    /// Since the L2 block executor is stateless, on an update to the safe head,
    /// a new executor is created with the updated header.
    fn update_safe_head(&mut self, header: Sealed<Header>) {
        self.inner = Some(StatelessL2Builder::new(
            self.rollup_config,
            self.evm_factory.clone(),
            self.trie_provider.clone(),
            self.trie_hinter.clone(),
            header,
        ));
    }

    /// Execute the given payload attributes.
    async fn execute_payload(
        &mut self,
        attributes: BasePayloadAttributes,
    ) -> Result<BlockBuildingOutcome, Self::Error> {
        self.inner.as_mut().map_or_else(
            || Err(crate::ExecutorError::MissingExecutor),
            |e| e.build_block(attributes),
        )
    }

    /// Computes the output root.
    fn compute_output_root(&mut self) -> Result<B256, Self::Error> {
        self.inner
            .as_mut()
            .map_or_else(|| Err(crate::ExecutorError::MissingExecutor), |e| e.compute_output_root())
    }
}
