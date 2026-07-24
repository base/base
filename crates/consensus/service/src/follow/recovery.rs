//! Follow-mode recovery: when the local chain diverges from the source, reset the local engine to
//! the highest block both nodes agree on (the common ancestor) and let the normal insert loop
//! replay source payloads forward from there. The reset targets a block the local EL already has,
//! so the forkchoice update is `Valid` and the head reorgs without EL sync.

use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::time;
use tracing::info;

use crate::follow::{
    engine::FollowEngine, error::FollowError, local::FollowLocalClient, source::RemoteClient,
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

/// The local reset point and the captured source-safe branch to replay from it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct RecoveryPlan {
    /// Highest block shared by the local and source chains.
    pub(super) ancestor: L2BlockInfo,
    /// Source blocks after `ancestor` through the captured safe head, ordered from oldest to newest.
    pub(super) replay: Vec<ReplayBlock>,
}

#[derive(Debug)]
struct RecoveryBudget {
    deadline: Instant,
    lookups: u64,
    max_lookups: u64,
    walk_depth: u64,
}

impl RecoveryBudget {
    fn new() -> Self {
        Self::with_limits(RECOVERY_DEADLINE, MAX_RECOVERY_LOOKUPS)
    }

    fn with_limits(deadline: Duration, max_lookups: u64) -> Self {
        Self { deadline: Instant::now() + deadline, lookups: 0, max_lookups, walk_depth: 0 }
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
    source_safe: &BlockInfo,
    divergence_local_hash: B256,
) -> Result<RecoveryPlan, FollowError>
where
    Local: FollowLocalClient,
    Remote: RemoteClient,
{
    let started_at = Instant::now();
    let mut budget = RecoveryBudget::new();
    let finalized = budget
        .run("finalized fence", local.block_info(BlockNumberOrTag::Finalized))
        .await?
        .ok_or(FollowError::LocalBlockUnavailable(BlockNumberOrTag::Finalized))?;
    let plan = build_recovery_plan(local, source, &finalized, source_safe, &mut budget).await?;

    let fresh_finalized = budget
        .run("fresh finalized fence", local.block_info(BlockNumberOrTag::Finalized))
        .await?
        .ok_or(FollowError::LocalBlockUnavailable(BlockNumberOrTag::Finalized))?;
    if fresh_finalized.block_info.number != finalized.block_info.number
        || fresh_finalized.block_info.hash != finalized.block_info.hash
    {
        return Err(FollowError::FinalizedHeadChanged {
            previous_number: finalized.block_info.number,
            previous_hash: finalized.block_info.hash,
            current_number: fresh_finalized.block_info.number,
            current_hash: fresh_finalized.block_info.hash,
        });
    }

    engine.reset_to_ancestor(plan.ancestor).await?;
    info!(
        target: "follow",
        divergence_number = source_safe.number,
        divergence_local = %divergence_local_hash,
        divergence_source = %source_safe.hash,
        ancestor_number = plan.ancestor.block_info.number,
        ancestor_hash = %plan.ancestor.block_info.hash,
        source_safe_number = source_safe.number,
        source_safe_hash = %source_safe.hash,
        walk_depth = budget.walk_depth,
        replay_blocks = plan.replay.len(),
        recovery_lookups = budget.lookups,
        duration = ?started_at.elapsed(),
        "Prepared source-safe follow recovery",
    );
    Ok(plan)
}

/// Walks the captured source-safe branch backward and returns the highest matching local ancestor.
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
    Ok(build_recovery_plan(local, source, finalized, source_safe, &mut budget).await?.ancestor)
}

/// Walks the source-safe branch backward by parent hash, collecting the source-safe replay path,
/// and returns its highest block that matches the local chain. The finalized head must be common;
/// recovery cannot reorg below it.
async fn build_recovery_plan<Local, Remote>(
    local: &Local,
    source: &Remote,
    finalized: &L2BlockInfo,
    source_safe: &BlockInfo,
    budget: &mut RecoveryBudget,
) -> Result<RecoveryPlan, FollowError>
where
    Local: FollowLocalClient,
    Remote: RemoteClient,
{
    let finalized_number = finalized.block_info.number;
    let mut source_block = *source_safe;
    let mut reverse_replay = Vec::new();

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
                reverse_replay.reverse();
                return Ok(RecoveryPlan { ancestor: *finalized, replay: reverse_replay });
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
            reverse_replay.reverse();
            return Ok(RecoveryPlan { ancestor: local_block, replay: reverse_replay });
        }

        reverse_replay.push(ReplayBlock {
            number: source_block.number,
            hash: source_block.hash,
            parent_hash: source_block.parent_hash,
        });
        let parent = budget
            .run("recovery parent lookup", async {
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
        budget.walk_depth = budget.walk_depth.saturating_add(1);
        source_block = parent;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FollowError, RecoveryBudget};

    #[tokio::test]
    async fn recovery_budget_bounds_one_walk() {
        let mut budget = RecoveryBudget::with_limits(Duration::from_secs(1), 2);

        budget
            .run("recovery", async { Ok::<_, FollowError>(()) })
            .await
            .expect("first lookup should fit within budget");
        budget
            .run("recovery", async { Ok::<_, FollowError>(()) })
            .await
            .expect("second lookup should fit within budget");

        let error = budget
            .run("recovery", async { Ok::<_, FollowError>(()) })
            .await
            .expect_err("lookup budget should be exhausted");
        assert!(matches!(
            error,
            FollowError::RecoveryBudgetExceeded { phase: "recovery", lookups: 2 }
        ));
    }
}
