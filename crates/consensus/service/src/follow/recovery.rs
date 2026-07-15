//! Follow-mode recovery: when the local chain diverges from the source, reset the local engine to
//! the highest block both nodes agree on (the common ancestor) and let the normal insert loop
//! replay source payloads forward from there. The reset targets a block the local EL already has,
//! so its forkchoice update is `Valid` (no EL sync) — unlike pointing the EL at the canonical tip,
//! which it lacks and would answer `Syncing`.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_protocol::{BlockInfo, L2BlockInfo};

use crate::follow::{
    engine::FollowEngine, error::FollowError, local::FollowLocalClient, source::RemoteClient,
};

/// A source block to replay after resetting to a common ancestor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReplayBlock {
    /// Source block number.
    pub(super) number: u64,
    /// Source block hash.
    pub(super) hash: B256,
    /// Source block parent hash.
    pub(super) parent_hash: B256,
}

/// The local reset point and the source branch to replay from it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct RecoveryPlan {
    /// Highest block shared by the local and source chains.
    pub(super) ancestor: L2BlockInfo,
    /// Source blocks after `ancestor`, ordered from oldest to newest.
    pub(super) replay: Vec<ReplayBlock>,
}

/// Resets the local engine onto the common ancestor of the local and source chains and returns the
/// source branch to replay. `source_safe` is the coherent source label block where the two chains
/// are known to disagree.
pub(super) async fn recover<Local, Remote>(
    local: &Local,
    source: &Remote,
    engine: &Arc<dyn FollowEngine>,
    finalized: &L2BlockInfo,
    source_safe: &BlockInfo,
) -> Result<RecoveryPlan, FollowError>
where
    Local: FollowLocalClient,
    Remote: RemoteClient,
{
    let ancestor = find_common_ancestor(local, source, finalized, source_safe).await?;
    let source_latest = source.get_block_info(BlockNumberOrTag::Latest).await?;
    let replay = find_replay_path(source, source_latest, source_safe, &ancestor).await?;
    engine.reset_to_ancestor(ancestor).await?;
    Ok(RecoveryPlan { ancestor, replay })
}

/// Walks the captured source-latest branch backward to the common ancestor and returns it in
/// replay order. The source-safe block must occur on this branch; otherwise the source reads were
/// not coherent and replay must not begin.
async fn find_replay_path<Remote>(
    source: &Remote,
    source_latest: BlockInfo,
    source_safe: &BlockInfo,
    ancestor: &L2BlockInfo,
) -> Result<Vec<ReplayBlock>, FollowError>
where
    Remote: RemoteClient,
{
    let mut source_block = source_latest;
    let mut reverse_path = Vec::new();
    let mut saw_source_safe = source_safe.number == ancestor.block_info.number
        && source_safe.hash == ancestor.block_info.hash;

    loop {
        if source_block.number < ancestor.block_info.number {
            return Err(FollowError::SourceChainDiscontinuity {
                child_number: source_latest.number,
                parent_number: source_block.number,
            });
        }

        if source_block.number == source_safe.number {
            if source_block.hash != source_safe.hash {
                return Err(FollowError::SourceBranchMismatch {
                    number: source_block.number,
                    expected: source_safe.hash,
                    actual: source_block.hash,
                });
            }
            saw_source_safe = true;
        }

        if source_block.number == ancestor.block_info.number {
            if source_block.hash != ancestor.block_info.hash {
                return Err(FollowError::SourceBranchMismatch {
                    number: source_block.number,
                    expected: ancestor.block_info.hash,
                    actual: source_block.hash,
                });
            }
            break;
        }

        reverse_path.push(ReplayBlock {
            number: source_block.number,
            hash: source_block.hash,
            parent_hash: source_block.parent_hash,
        });

        let parent = source.get_block_info_by_hash(source_block.parent_hash).await?;
        if !parent.is_parent_of(&source_block) {
            return Err(FollowError::SourceChainDiscontinuity {
                child_number: source_block.number,
                parent_number: parent.number,
            });
        }
        source_block = parent;
    }

    if !saw_source_safe {
        return Err(FollowError::SourceChainDiscontinuity {
            child_number: source_latest.number,
            parent_number: source_safe.number,
        });
    }

    reverse_path.reverse();
    Ok(reverse_path)
}

/// Walks the captured source-safe branch backward by parent hash and returns its highest block that
/// matches the local chain. The finalized head must be common; recovery cannot reorg below it.
///
/// Parent-hash traversal pins every lookup to the branch containing `source_safe`. Independent
/// by-number lookups are unsafe here because a load-balanced source can route them to replicas on
/// different branches and violate the monotonicity required by a binary search.
pub(super) async fn find_common_ancestor<Local, Remote>(
    local: &Local,
    source: &Remote,
    finalized: &L2BlockInfo,
    source_safe: &BlockInfo,
) -> Result<L2BlockInfo, FollowError>
where
    Local: FollowLocalClient,
    Remote: RemoteClient,
{
    let finalized_number = finalized.block_info.number;
    let mut source_block = *source_safe;

    loop {
        let number = source_block.number;
        if number < finalized_number {
            return Err(FollowError::SourceChainDiscontinuity {
                child_number: source_safe.number,
                parent_number: number,
            });
        }

        if number == finalized_number {
            if source_block.hash == finalized.block_info.hash {
                return Ok(*finalized);
            }
            return Err(FollowError::SourceBlockHashMismatch {
                number,
                local: finalized.block_info.hash,
                remote: source_block.hash,
            });
        }

        if let Some(local_block) = local.block_info(number.into()).await?
            && local_block.block_info.hash == source_block.hash
        {
            return Ok(local_block);
        }

        let parent = source.get_block_info_by_hash(source_block.parent_hash).await?;
        if !parent.is_parent_of(&source_block) {
            return Err(FollowError::SourceChainDiscontinuity {
                child_number: number,
                parent_number: parent.number,
            });
        }
        source_block = parent;
    }
}
