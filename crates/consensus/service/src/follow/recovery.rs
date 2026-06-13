//! Follow-mode recovery: when the local chain diverges from the source, reset the local engine to
//! the highest block both nodes agree on (the common ancestor) and let the normal insert loop
//! replay source payloads forward from there. The reset targets a block the local EL already has,
//! so its forkchoice update is `Valid` (no EL sync) — unlike pointing the EL at the canonical tip,
//! which it lacks and would answer `Syncing`.

use std::sync::Arc;

use base_protocol::L2BlockInfo;

use crate::follow::{
    engine::FollowEngine, error::FollowError, local::FollowLocalClient, source::RemoteClient,
};

/// Resets the local engine onto the common ancestor of the local and source chains and returns the
/// ancestor block number, from which fetch/insert should restart. `divergent_number` is a block
/// number (at or below the local latest) where the two chains are known to disagree.
pub(super) async fn recover<Local, Remote>(
    local: &Local,
    source: &Remote,
    engine: &Arc<dyn FollowEngine>,
    finalized: &L2BlockInfo,
    divergent_number: u64,
) -> Result<u64, FollowError>
where
    Local: FollowLocalClient,
    Remote: RemoteClient,
{
    let ancestor = find_common_ancestor(local, source, finalized, divergent_number).await?;
    engine.reset_to_ancestor(ancestor).await?;
    Ok(ancestor.block_info.number)
}

/// Returns the highest L2 block in `[finalized, divergent_number)` whose hash is the same on the
/// local and source nodes. The finalized head must itself be common; if it is not, recovery cannot
/// safely reorg below it.
///
/// Forks are contiguous (the chains share a prefix and then diverge), so "agrees with source" is
/// monotonic in the block number, which makes a binary search correct.
pub(super) async fn find_common_ancestor<Local, Remote>(
    local: &Local,
    source: &Remote,
    finalized: &L2BlockInfo,
    divergent_number: u64,
) -> Result<L2BlockInfo, FollowError>
where
    Local: FollowLocalClient,
    Remote: RemoteClient,
{
    // The search floor must be common. Finality is never reorged, so stop before resetting.
    let finalized_number = finalized.block_info.number;
    let source_finalized = source.get_block_info(finalized_number.into()).await?;
    if source_finalized.hash != finalized.block_info.hash {
        return Err(FollowError::SourceBlockHashMismatch {
            number: finalized_number,
            local: finalized.block_info.hash,
            remote: source_finalized.hash,
        });
    }

    // Invariant: blocks agree at `lo`, disagree at `hi`. Converge to the highest agreeing block.
    let mut lo = finalized_number;
    let mut hi = divergent_number.max(finalized_number + 1);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if blocks_match(local, source, mid).await? {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    local.block_info(lo.into()).await?.ok_or(FollowError::LocalBlockUnavailable(lo.into()))
}

/// Whether the local and source nodes report the same hash for the block at `number`. A missing
/// local block counts as a mismatch (the search moves below it).
async fn blocks_match<Local, Remote>(
    local: &Local,
    source: &Remote,
    number: u64,
) -> Result<bool, FollowError>
where
    Local: FollowLocalClient,
    Remote: RemoteClient,
{
    let Some(local_block) = local.block_info(number.into()).await? else {
        return Ok(false);
    };
    let source_block = source.get_block_info(number.into()).await?;
    Ok(local_block.block_info.hash == source_block.hash)
}
