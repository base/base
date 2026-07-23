//! Follow-mode recovery via Engine API forkchoice sync.
//!
//! On safe-head divergence, prove the current source-safe branch still contains the local
//! finalized hash, then send a source-accurate forkchoice update. The local EL is expected to
//! backfill missing bodies from peers; follow does not replay payloads with `newPayload` during
//! recovery.

use std::{sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::follow::{
    engine::FollowEngine,
    error::FollowError,
    local::{FollowLocalClient, validate_block_l1_origin},
    source::RemoteClient,
};

const FINALIZED_LOOKUP_LIMIT: usize = 4_096;
const HEAD_SYNC_TIMEOUT: Duration = Duration::from_secs(120);
const HEAD_SYNC_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(super) struct FollowRecovery;

impl FollowRecovery {
    /// Reads fresh source-safe and local finalized labels, then recovers onto that source-safe head.
    pub(super) async fn recover<Local, Remote>(
        local: Arc<Local>,
        source: Arc<Remote>,
        engine: Arc<dyn FollowEngine>,
        cancellation: CancellationToken,
    ) -> Result<L2BlockInfo, FollowError>
    where
        Local: FollowLocalClient + 'static,
        Remote: RemoteClient + 'static,
    {
        let source_safe = source.get_block_info(BlockNumberOrTag::Safe).await?;
        let finalized = local
            .block_info(BlockNumberOrTag::Finalized)
            .await?
            .ok_or(FollowError::LocalBlockUnavailable(BlockNumberOrTag::Finalized))?;

        Self::ensure_finalized_on_source_branch(
            source.as_ref(),
            &cancellation,
            source_safe,
            &finalized,
        )
        .await?;

        let deadline = Instant::now() + HEAD_SYNC_TIMEOUT;
        let mut attempts = 0_u64;
        loop {
            if Instant::now() >= deadline {
                return Err(FollowError::RecoveryFailed(
                    "execution layer did not confirm source-safe head",
                ));
            }
            attempts = attempts.saturating_add(1);

            // Keep safe/finalized pinned to the verified local finalized fence while the EL may
            // still be Syncing. Promoting source-safe as safe is deferred until Valid confirmation.
            let confirmed = tokio::select! {
                _ = cancellation.cancelled() => return Err(FollowError::RecoveryCancelled),
                result = engine.request_forkchoice(
                    source_safe,
                    finalized.block_info,
                    finalized.block_info,
                ) => result?,
            };
            if confirmed {
                break;
            }

            tokio::select! {
                _ = cancellation.cancelled() => return Err(FollowError::RecoveryCancelled),
                _ = time::sleep(HEAD_SYNC_POLL_INTERVAL) => {}
            }
        }

        let recovered = local.block_info_by_hash(source_safe.hash).await?.ok_or(
            FollowError::RecoveryFailed(
                "confirmed source-safe head was unavailable from the local execution layer",
            ),
        )?;
        if recovered.block_info != source_safe {
            return Err(FollowError::SourceBlockHashMismatch {
                number: source_safe.number,
                local: recovered.block_info.hash,
                remote: source_safe.hash,
            });
        }
        validate_block_l1_origin(local.as_ref(), &recovered).await?;

        engine.update_safe_finalized_blocks(Some(recovered), None).await?;
        info!(
            target: "follow",
            number = recovered.block_info.number,
            hash = %recovered.block_info.hash,
            finalized_number = finalized.block_info.number,
            finalized_hash = %finalized.block_info.hash,
            attempts,
            "Recovered follow mode onto source-safe head via forkchoice sync",
        );
        Ok(recovered)
    }

    /// Walks the source-safe branch by parent hash until the local finalized hash appears.
    async fn ensure_finalized_on_source_branch<Remote>(
        source: &Remote,
        cancellation: &CancellationToken,
        source_safe: BlockInfo,
        finalized: &L2BlockInfo,
    ) -> Result<(), FollowError>
    where
        Remote: RemoteClient,
    {
        let finalized_number = finalized.block_info.number;
        let finalized_hash = finalized.block_info.hash;
        let mut source_block = source_safe;
        let mut lookups = 0;

        loop {
            if source_block.hash == finalized_hash {
                if source_block.number != finalized_number {
                    return Err(FollowError::RecoveryFailed(
                        "source finalized hash appeared at an unexpected block number",
                    ));
                }
                return Ok(());
            }
            if source_block.number <= finalized_number {
                return Err(FollowError::FinalizedDivergence {
                    number: finalized_number,
                    local: finalized_hash,
                    remote: source_block.hash,
                });
            }
            if lookups >= FINALIZED_LOOKUP_LIMIT {
                return Err(FollowError::RecoveryFailed(
                    "exceeded finalized-ancestry lookup budget",
                ));
            }
            lookups += 1;

            let parent = tokio::select! {
                _ = cancellation.cancelled() => return Err(FollowError::RecoveryCancelled),
                result = source.get_block_info_by_hash(source_block.parent_hash) => result?,
            };
            if parent.hash != source_block.parent_hash
                || parent.number.saturating_add(1) != source_block.number
            {
                return Err(FollowError::RecoveryFailed(
                    "source parent lookup did not return the claimed parent",
                ));
            }
            source_block = parent;
        }
    }
}
