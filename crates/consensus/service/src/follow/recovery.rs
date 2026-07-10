//! Follow-mode recovery: when the local chain diverges from the source, reset the local engine to
//! the highest block both nodes agree on (the common ancestor) and let the normal insert loop
//! replay source payloads forward from there. The reset targets a block the local EL already has,
//! so its forkchoice update is `Valid` (no EL sync) — unlike pointing the EL at the canonical tip,
//! which it lacks and would answer `Syncing`.

use std::sync::Arc;

use base_protocol::{BlockInfo, L2BlockInfo};

use crate::follow::{
    engine::FollowEngine, error::FollowError, local::FollowLocalClient, source::RemoteClient,
};

/// Resets the local engine onto the common ancestor of the local and source chains and returns the
/// ancestor block number, from which fetch/insert should restart. `source_safe` is the coherent
/// source label block where the two chains are known to disagree.
pub(super) async fn recover<Local, Remote>(
    local: &Local,
    source: &Remote,
    engine: &Arc<dyn FollowEngine>,
    finalized: &L2BlockInfo,
    source_safe: &BlockInfo,
) -> Result<u64, FollowError>
where
    Local: FollowLocalClient,
    Remote: RemoteClient,
{
    let ancestor = find_common_ancestor(local, source, finalized, source_safe).await?;
    engine.reset_to_ancestor(ancestor).await?;
    Ok(ancestor.block_info.number)
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
