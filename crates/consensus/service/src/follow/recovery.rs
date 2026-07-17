//! Follow-mode recovery: when the local chain diverges from the source, reset the local engine to
//! the highest block both nodes agree on (the common ancestor) and let the normal insert loop
//! replay source payloads forward from there. The reset targets a block the local EL already has,
//! so its forkchoice update is `Valid` (no EL sync) — unlike pointing the EL at the canonical tip,
//! which it lacks and would answer `Syncing`.

use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::time;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::follow::{
    engine::{FollowEngine, ResetStats},
    error::FollowError,
    local::FollowLocalClient,
    source::RemoteClient,
};

const RECOVERY_DEADLINE: Duration = Duration::from_secs(30);
const MAX_RECOVERY_LOOKUPS: u64 = 4096;

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

#[derive(Debug)]
struct RecoveryBudget {
    deadline: Instant,
    lookups: u64,
    max_lookups: u64,
    ancestor_walk_depth: u64,
    replay_walk_depth: u64,
}

impl RecoveryBudget {
    fn new() -> Self {
        Self::with_limits(RECOVERY_DEADLINE, MAX_RECOVERY_LOOKUPS)
    }

    fn with_limits(deadline: Duration, max_lookups: u64) -> Self {
        Self {
            deadline: Instant::now() + deadline,
            lookups: 0,
            max_lookups,
            ancestor_walk_depth: 0,
            replay_walk_depth: 0,
        }
    }

    fn next_timeout(&mut self, phase: &'static str) -> Result<Duration, FollowError> {
        if self.lookups >= self.max_lookups {
            return Err(FollowError::RecoveryBudgetExceeded { phase, lookups: self.lookups });
        }
        self.lookups += 1;
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FollowError::RecoveryBudgetExceeded { phase, lookups: self.lookups });
        }
        Ok(remaining)
    }

    async fn run<T, Operation>(
        &mut self,
        phase: &'static str,
        operation: Operation,
    ) -> Result<T, FollowError>
    where
        Operation: Future<Output = Result<T, FollowError>>,
    {
        let timeout = self.next_timeout(phase)?;
        time::timeout(timeout, operation)
            .await
            .map_err(|_| FollowError::RecoveryBudgetExceeded { phase, lookups: self.lookups })?
    }
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
    divergence_local_hash: B256,
    cancellation: CancellationToken,
) -> Result<RecoveryPlan, FollowError>
where
    Local: FollowLocalClient,
    Remote: RemoteClient,
{
    let started_at = Instant::now();
    let mut budget = RecoveryBudget::new();
    let (mut ancestor, mut source_latest, mut replay) =
        build_recovery_path(local, source, finalized, source_safe, &mut budget).await?;

    let fresh_finalized = budget
        .run("fresh finalized fence", local.block_info(BlockNumberOrTag::Finalized))
        .await?
        .ok_or(FollowError::LocalBlockUnavailable(BlockNumberOrTag::Finalized))?;
    if fresh_finalized.block_info.number != finalized.block_info.number
        || fresh_finalized.block_info.hash != finalized.block_info.hash
    {
        (ancestor, source_latest, replay) =
            build_recovery_path(local, source, &fresh_finalized, source_safe, &mut budget).await?;
    }

    let reset_stats: ResetStats = engine.reset_to_ancestor(ancestor, cancellation.clone()).await?;
    if !cancellation.is_cancelled() {
        info!(
            target: "follow",
            divergence_number = source_safe.number,
            divergence_local = %divergence_local_hash,
            divergence_source = %source_safe.hash,
            ancestor_number = ancestor.block_info.number,
            ancestor_hash = %ancestor.block_info.hash,
            source_safe_number = source_safe.number,
            source_safe_hash = %source_safe.hash,
            source_latest_number = source_latest.number,
            source_latest_hash = %source_latest.hash,
            walk_depth = budget.ancestor_walk_depth + budget.replay_walk_depth,
            ancestor_walk_depth = budget.ancestor_walk_depth,
            replay_walk_depth = budget.replay_walk_depth,
            replay_blocks = replay.len(),
            recovery_lookups = budget.lookups,
            reset_retries = reset_stats.retries,
            duration = ?started_at.elapsed(),
            "Prepared source-coherent follow recovery",
        );
    }
    Ok(RecoveryPlan { ancestor, replay })
}

async fn build_recovery_path<Local, Remote>(
    local: &Local,
    source: &Remote,
    finalized: &L2BlockInfo,
    source_safe: &BlockInfo,
    budget: &mut RecoveryBudget,
) -> Result<(L2BlockInfo, BlockInfo, Vec<ReplayBlock>), FollowError>
where
    Local: FollowLocalClient,
    Remote: RemoteClient,
{
    let ancestor =
        find_common_ancestor_with_budget(local, source, finalized, source_safe, budget).await?;
    let source_latest = budget
        .run("source latest lookup", async {
            source.get_block_info(BlockNumberOrTag::Latest).await.map_err(FollowError::from)
        })
        .await?;
    let replay = find_replay_path(source, source_latest, source_safe, &ancestor, budget).await?;
    Ok((ancestor, source_latest, replay))
}

/// Walks the captured source-latest branch backward to the common ancestor and returns it in
/// replay order. The source-safe block must occur on this branch; otherwise the source reads were
/// not coherent and replay must not begin.
async fn find_replay_path<Remote>(
    source: &Remote,
    source_latest: BlockInfo,
    source_safe: &BlockInfo,
    ancestor: &L2BlockInfo,
    budget: &mut RecoveryBudget,
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

        let parent = budget
            .run("replay path parent lookup", async {
                source
                    .get_block_info_by_hash(source_block.parent_hash)
                    .await
                    .map_err(FollowError::from)
            })
            .await?;
        if !parent.is_parent_of(&source_block) {
            return Err(FollowError::SourceChainDiscontinuity {
                child_number: source_block.number,
                parent_number: parent.number,
            });
        }
        budget.replay_walk_depth = budget.replay_walk_depth.saturating_add(1);
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
#[cfg(test)]
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
    let mut budget = RecoveryBudget::new();
    find_common_ancestor_with_budget(local, source, finalized, source_safe, &mut budget).await
}

async fn find_common_ancestor_with_budget<Local, Remote>(
    local: &Local,
    source: &Remote,
    finalized: &L2BlockInfo,
    source_safe: &BlockInfo,
    budget: &mut RecoveryBudget,
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
            return Err(FollowError::FinalizedDivergence {
                number,
                local: finalized.block_info.hash,
                remote: source_block.hash,
            });
        }

        if let Some(local_block) =
            budget.run("ancestor local lookup", local.block_info(number.into())).await?
            && local_block.block_info.hash == source_block.hash
        {
            return Ok(local_block);
        }

        let parent = budget
            .run("ancestor parent lookup", async {
                source
                    .get_block_info_by_hash(source_block.parent_hash)
                    .await
                    .map_err(FollowError::from)
            })
            .await?;
        if !parent.is_parent_of(&source_block) {
            return Err(FollowError::SourceChainDiscontinuity {
                child_number: number,
                parent_number: parent.number,
            });
        }
        budget.ancestor_walk_depth = budget.ancestor_walk_depth.saturating_add(1);
        source_block = parent;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FollowError, RecoveryBudget};

    #[tokio::test]
    async fn recovery_budget_is_shared_across_phases() {
        let mut budget = RecoveryBudget::with_limits(Duration::from_secs(1), 2);

        budget
            .run("ancestor", async { Ok::<_, FollowError>(()) })
            .await
            .expect("ancestor lookup should fit within budget");
        budget
            .run("replay", async { Ok::<_, FollowError>(()) })
            .await
            .expect("replay lookup should fit within budget");

        let error = budget
            .run("replay", async { Ok::<_, FollowError>(()) })
            .await
            .expect_err("shared lookup budget should be exhausted");
        assert!(matches!(
            error,
            FollowError::RecoveryBudgetExceeded { phase: "replay", lookups: 2 }
        ));
    }
}
