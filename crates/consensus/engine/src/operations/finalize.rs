//! Direct L2 finalization operations.

use std::{sync::Arc, time::Instant};

use alloy_transport::{RpcError, TransportErrorKind};
use base_common_genesis::RollupConfig;
use base_protocol::{FromBlockError, L2BlockInfo};
use thiserror::Error;

use crate::{
    Engine, EngineClient, EngineState, EngineSyncStateUpdate, EngineTaskError,
    EngineTaskErrorSeverity, Metrics, SynchronizeTask, SynchronizeTaskError,
};

/// An error that occurs when running [`crate::Engine::finalize`].
#[derive(Debug, Error)]
pub enum FinalizeTaskError {
    /// The block is not safe, and therefore cannot be finalized.
    #[error("Attempted to finalize a block that is not yet safe")]
    BlockNotSafe,
    /// The block to finalize was not found.
    #[error("The block to finalize was not found: Number {0}")]
    BlockNotFound(u64),
    /// An error occurred while transforming the RPC block into [`L2BlockInfo`].
    ///
    /// [`L2BlockInfo`]: base_protocol::L2BlockInfo
    #[error(transparent)]
    FromBlock(#[from] FromBlockError),
    /// A temporary RPC failure.
    #[error(transparent)]
    TransportError(#[from] RpcError<TransportErrorKind>),
    /// The forkchoice update call to finalize the block failed.
    #[error(transparent)]
    ForkchoiceUpdateFailed(#[from] SynchronizeTaskError),
}

impl EngineTaskError for FinalizeTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::BlockNotSafe | Self::BlockNotFound(_) | Self::FromBlock(_) => {
                EngineTaskErrorSeverity::Critical
            }
            Self::TransportError(_) => EngineTaskErrorSeverity::Temporary,
            Self::ForkchoiceUpdateFailed(inner) => inner.severity(),
        }
    }
}

impl Engine {
    /// Finalizes an L2 block directly against the execution layer.
    pub async fn finalize<EngineClient_>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        block_number: u64,
    ) -> Result<(), FinalizeTaskError>
    where
        EngineClient_: EngineClient + 'static,
    {
        self.retry_with_severity(Metrics::FINALIZE_TASK_LABEL, move |state| {
            let client = Arc::clone(&client);
            let config = Arc::clone(&config);
            Box::pin(
                async move { Self::finalize_with_state(state, client, config, block_number).await },
            )
        })
        .await
    }

    /// Finalizes an L2 block using the provided engine state.
    pub async fn finalize_with_state<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        block_number: u64,
    ) -> Result<(), FinalizeTaskError> {
        let current_finalized = state.sync_state.finalized_head().block_info.number;
        if block_number < current_finalized {
            debug!(
                target: "engine",
                block_number,
                current_finalized,
                "Skipping stale finalized update"
            );
            return Ok(());
        }

        if state.sync_state.safe_head().block_info.number < block_number {
            return Err(FinalizeTaskError::BlockNotSafe);
        }

        let block_fetch_start = Instant::now();
        let block = client
            .get_l2_block(block_number.into())
            .full()
            .await
            .map_err(FinalizeTaskError::TransportError)?
            .ok_or(FinalizeTaskError::BlockNotFound(block_number))?
            .into_consensus();
        let block_info = L2BlockInfo::from_block_and_genesis(
            &block.map_transactions(|tx| tx.inner.inner.into_inner()),
            &client.cfg().genesis,
        )
        .map_err(FinalizeTaskError::FromBlock)?;
        let block_fetch_duration = block_fetch_start.elapsed();

        let fcu_start = Instant::now();
        SynchronizeTask::new(
            client,
            config,
            EngineSyncStateUpdate { finalized_head: Some(block_info), ..Default::default() },
        )
        .execute(state)
        .await?;
        let fcu_duration = fcu_start.elapsed();
        let total_duration = block_fetch_start.elapsed();
        Metrics::engine_finalize_duration_seconds().record(total_duration.as_secs_f64());

        info!(
            target: "engine",
            hash = %block_info.block_info.hash,
            number = block_info.block_info.number,
            ?block_fetch_duration,
            ?fcu_duration,
            "Updated finalized head"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag};
    use alloy_primitives::{B256, b256};
    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
    use alloy_rpc_types_eth::Block as RpcBlock;
    use base_common_genesis::{ChainGenesis, RollupConfig};
    use base_common_rpc_types::Transaction as BaseTransaction;
    use rstest::rstest;

    use super::*;
    use crate::test_utils::{TestEngineStateBuilder, test_block_info, test_engine_client_builder};

    const BASE_SEPOLIA_GENESIS_HASH: B256 =
        b256!("0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4");

    const BASE_MAINNET_GENESIS_HASH: B256 =
        b256!("f712aa9241cc24369b143cf6dce85f0902a9731e70d66818a3a5845b296c73dd");

    fn make_genesis_block() -> (RpcBlock<BaseTransaction>, B256) {
        let block = RpcBlock::<BaseTransaction>::default();
        let hash = block.clone().into_consensus().hash_slow();
        (block, hash)
    }

    fn genesis_rollup_cfg(hash: B256) -> Arc<RollupConfig> {
        Arc::new(RollupConfig {
            genesis: ChainGenesis { l2: BlockNumHash { number: 0, hash }, ..Default::default() },
            ..Default::default()
        })
    }

    fn valid_fcu(hash: B256) -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: Some(hash),
            },
            payload_id: None,
        }
    }

    #[derive(Debug)]
    enum ExpectedFinalizeError {
        BlockNotSafe,
        BlockNotFound(u64),
        FromBlock,
        ForkchoiceUpdateFailed,
    }

    impl ExpectedFinalizeError {
        fn matches(&self, result: &Result<(), FinalizeTaskError>) -> bool {
            match (self, result) {
                (Self::BlockNotFound(expected), Err(FinalizeTaskError::BlockNotFound(actual))) => {
                    expected == actual
                }
                (Self::BlockNotSafe, Err(FinalizeTaskError::BlockNotSafe))
                | (Self::FromBlock, Err(FinalizeTaskError::FromBlock(_)))
                | (
                    Self::ForkchoiceUpdateFailed,
                    Err(FinalizeTaskError::ForkchoiceUpdateFailed(_)),
                ) => true,
                _ => false,
            }
        }
    }

    #[derive(Debug)]
    enum GenesisFinalizeFailure {
        HashMismatch,
        MissingFcu,
    }

    #[rstest]
    #[case::block_not_safe(5, 10, ExpectedFinalizeError::BlockNotSafe)]
    #[case::block_not_found(10, 7, ExpectedFinalizeError::BlockNotFound(7))]
    #[tokio::test]
    async fn direct_finalize_block_validation_errors(
        #[case] safe_head: u64,
        #[case] block_number: u64,
        #[case] expected: ExpectedFinalizeError,
    ) {
        let client = test_engine_client_builder().build();
        let head = test_block_info(safe_head);
        let finalized = test_block_info(0);
        let mut state = TestEngineStateBuilder::new()
            .with_safe_head(head)
            .with_unsafe_head(head)
            .with_finalized_head(finalized)
            .build();

        let result = Engine::finalize_with_state(
            &mut state,
            Arc::new(client),
            Arc::new(RollupConfig::default()),
            block_number,
        )
        .await;

        assert!(expected.matches(&result), "expected {expected:?}, got {result:?}");
    }

    #[rstest]
    #[case::genesis_hash_mismatch(
        GenesisFinalizeFailure::HashMismatch,
        ExpectedFinalizeError::FromBlock
    )]
    #[case::missing_fcu(
        GenesisFinalizeFailure::MissingFcu,
        ExpectedFinalizeError::ForkchoiceUpdateFailed
    )]
    #[tokio::test]
    async fn direct_finalize_genesis_errors(
        #[case] failure: GenesisFinalizeFailure,
        #[case] expected: ExpectedFinalizeError,
    ) {
        let (block, hash) = make_genesis_block();
        let cfg = match failure {
            GenesisFinalizeFailure::HashMismatch => genesis_rollup_cfg(BASE_SEPOLIA_GENESIS_HASH),
            GenesisFinalizeFailure::MissingFcu => genesis_rollup_cfg(hash),
        };

        let client = test_engine_client_builder()
            .with_config(Arc::clone(&cfg))
            .with_l2_block(BlockId::Number(BlockNumberOrTag::Number(0)), block)
            .build();
        let head = test_block_info(0);
        let mut state =
            TestEngineStateBuilder::new().with_safe_head(head).with_unsafe_head(head).build();

        let result = Engine::finalize_with_state(&mut state, Arc::new(client), cfg, 0).await;

        assert!(expected.matches(&result), "expected {expected:?}, got {result:?}");
    }

    #[tokio::test]
    async fn stale_finalize_does_not_regress_finalized_head() {
        let client = test_engine_client_builder().build();
        let head = test_block_info(10);
        let mut state = TestEngineStateBuilder::new()
            .with_unsafe_head(head)
            .with_safe_head(head)
            .with_finalized_head(head)
            .build();

        let result = Engine::finalize_with_state(
            &mut state,
            Arc::new(client),
            Arc::new(RollupConfig::default()),
            7,
        )
        .await;

        assert!(result.is_ok(), "stale finalize should succeed as a no-op, got {result:?}");
        assert_eq!(
            state.sync_state.finalized_head().block_info.number,
            10,
            "finalized_head must not regress",
        );
    }

    #[tokio::test]
    async fn success_updates_engine_state_finalized_head() {
        let (block, hash) = make_genesis_block();
        let cfg = genesis_rollup_cfg(hash);

        let client = test_engine_client_builder()
            .with_config(Arc::clone(&cfg))
            .with_l2_block(BlockId::Number(BlockNumberOrTag::Number(0)), block)
            .with_fork_choice_updated_v3_response(valid_fcu(BASE_MAINNET_GENESIS_HASH))
            .build();

        let mut state = TestEngineStateBuilder::new().build();

        Engine::finalize_with_state(&mut state, Arc::new(client), Arc::clone(&cfg), 0)
            .await
            .expect("finalization should succeed");

        assert_eq!(
            state.sync_state.finalized_head().block_info.hash,
            hash,
            "finalized_head hash must equal the genesis block hash after finalization",
        );
    }
}
