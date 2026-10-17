//! Group lifecycle precedence at its turn (design §5.7, normative).
//!
//! The builder applies these steps in order when the iterator yields a group; the first
//! matching step decides. Park and evict never mark the nonce committed.

use crate::{EvictionReason, FallbackMode};

/// Facts the builder gathers before deciding a group's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnContext {
    /// Block being built.
    pub target_block: u64,
    /// Signed inclusive expiry.
    pub expiry_block: u64,
    /// Whether the group ledger is available to this builder.
    pub ledger_available: bool,
    /// Whether the bound principal is still authorized.
    pub principal_authorized: bool,
    /// Whether selection is enabled (builder flag and runtime kill switch).
    pub selection_enabled: bool,
    /// Signed fallback mode.
    pub fallback: FallbackMode,
    /// Whether the group-owned budget cannot cover one more round.
    pub group_budget_exhausted: bool,
    /// Whether per-build cutoffs and the deadline reserve still admit a round.
    pub build_budget_available: bool,
}

/// Why a group is parked (kept pooled, lane held, no work spent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkReason {
    /// The ledger is unavailable; fail closed.
    LedgerUnavailable,
    /// Selection is disabled and the group is `DROP`.
    SelectionDisabled,
    /// No eligible sibling this round; `DROP` group with budget remaining.
    NoEligibleSibling,
}

/// Why a group is deferred to a later build without consuming an attempt or running fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferReason {
    /// Per-build wall or task-elapsed cutoff reached.
    BuildBudget,
    /// The principal's quota cannot cover the round.
    PrincipalQuota,
    /// Cancelled, or the deadline reserve is insufficient.
    Deadline,
    /// No sibling fits this block's remaining limits.
    BlockLimits,
    /// The winner's commit execution did not match its trial.
    VerifyMismatch,
}

/// Why the default sibling runs through the ordinary path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// Selection is runtime-disabled.
    SelectionDisabled,
    /// The group-owned budget is exhausted.
    BudgetExhausted,
    /// Evaluation found no eligible sibling.
    NoEligibleSibling,
}

/// The outcome of one lifecycle step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDecision {
    /// Remove the group from the pool without executing anything further.
    Evict(EvictionReason),
    /// Keep the group pooled until a later build or expiry.
    Park(ParkReason),
    /// Keep the group pooled; per-build limits stopped this turn.
    Defer(DeferReason),
    /// Run the default sibling through ordinary eligibility and the resource-checked commit.
    Fallback(FallbackReason),
    /// Reserve work and evaluate the siblings.
    Evaluate,
    /// Commit the selected winner.
    CommitWinner,
}

impl TurnDecision {
    /// Returns a stable metric label.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Evict(EvictionReason::Expired) => "expired",
            Self::Evict(EvictionReason::Revoked) => "revoked",
            Self::Evict(EvictionReason::BudgetExhausted) => "budget_exhausted",
            Self::Evict(EvictionReason::PermanentExclusion) => "permanent_exclusion",
            Self::Evict(EvictionReason::Dropped) => "dropped",
            Self::Park(ParkReason::LedgerUnavailable) => "ledger_unavailable",
            Self::Park(ParkReason::SelectionDisabled) => "disabled_parked",
            Self::Park(ParkReason::NoEligibleSibling) => "all_failed",
            Self::Defer(DeferReason::BuildBudget) => "deferred_build_budget",
            Self::Defer(DeferReason::PrincipalQuota) => "deferred_quota",
            Self::Defer(DeferReason::Deadline) => "deferred_deadline",
            Self::Defer(DeferReason::BlockLimits) => "deferred_block_limits",
            Self::Defer(DeferReason::VerifyMismatch) => "deferred_verify_mismatch",
            Self::Fallback(_) => "fallback",
            Self::Evaluate => "evaluate",
            Self::CommitWinner => "winner",
        }
    }
}

/// The normative group lifecycle.
#[derive(Debug, Clone, Copy, Default)]
pub struct GroupLifecycle;

impl GroupLifecycle {
    /// Steps 1–6: decide whether the group's turn evaluates, and if not, what happens.
    pub const fn before_evaluation(ctx: &TurnContext) -> TurnDecision {
        // 1. Hard expiry. Expiry never authorizes later execution.
        if ctx.target_block > ctx.expiry_block {
            return TurnDecision::Evict(EvictionReason::Expired);
        }
        // 2. Ledger unavailable: fail closed.
        if !ctx.ledger_available {
            return TurnDecision::Park(ParkReason::LedgerUnavailable);
        }
        // 3. Revoked principal: no further work, no fallback (design Q20 proposal).
        if !ctx.principal_authorized {
            return TurnDecision::Evict(EvictionReason::Revoked);
        }
        // 4. Selection runtime-disabled.
        if !ctx.selection_enabled {
            return match ctx.fallback {
                FallbackMode::DefaultSibling => {
                    TurnDecision::Fallback(FallbackReason::SelectionDisabled)
                }
                FallbackMode::Drop => TurnDecision::Park(ParkReason::SelectionDisabled),
            };
        }
        // 5. Group-owned budget exhausted.
        if ctx.group_budget_exhausted {
            return match ctx.fallback {
                FallbackMode::DefaultSibling => {
                    TurnDecision::Fallback(FallbackReason::BudgetExhausted)
                }
                FallbackMode::Drop => TurnDecision::Evict(EvictionReason::BudgetExhausted),
            };
        }
        // 6. Per-build cutoff or deadline reserve: defer in both modes.
        if !ctx.build_budget_available {
            return TurnDecision::Defer(DeferReason::BuildBudget);
        }
        // 7. Reserve and evaluate.
        TurnDecision::Evaluate
    }

    /// Step 8: decide what follows a completed evaluation round.
    pub const fn after_evaluation(
        has_winner: bool,
        fallback: FallbackMode,
        group_budget_remains: bool,
    ) -> TurnDecision {
        if has_winner {
            return TurnDecision::CommitWinner;
        }
        match fallback {
            FallbackMode::DefaultSibling => {
                TurnDecision::Fallback(FallbackReason::NoEligibleSibling)
            }
            FallbackMode::Drop if group_budget_remains => {
                TurnDecision::Park(ParkReason::NoEligibleSibling)
            }
            FallbackMode::Drop => TurnDecision::Evict(EvictionReason::BudgetExhausted),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(fallback: FallbackMode) -> TurnContext {
        TurnContext {
            target_block: 100,
            expiry_block: 100,
            ledger_available: true,
            principal_authorized: true,
            selection_enabled: true,
            fallback,
            group_budget_exhausted: false,
            build_budget_available: true,
        }
    }

    #[test]
    fn expiry_boundary_is_inclusive() {
        let ctx = ready(FallbackMode::DefaultSibling);
        assert_eq!(GroupLifecycle::before_evaluation(&ctx), TurnDecision::Evaluate);
        let expired = TurnContext { target_block: 101, ..ctx };
        assert_eq!(
            GroupLifecycle::before_evaluation(&expired),
            TurnDecision::Evict(EvictionReason::Expired)
        );
    }

    /// A blocking condition: name, how it changes the context, and the expected decision for
    /// `DEFAULT_SIBLING` and `DROP`.
    type Blocker = (&'static str, fn(&mut TurnContext), TurnDecision, TurnDecision);

    /// Each blocking condition, in precedence order.
    fn blockers() -> Vec<Blocker> {
        vec![
            (
                "expired",
                |ctx| ctx.target_block = ctx.expiry_block + 1,
                TurnDecision::Evict(EvictionReason::Expired),
                TurnDecision::Evict(EvictionReason::Expired),
            ),
            (
                "ledger",
                |ctx| ctx.ledger_available = false,
                TurnDecision::Park(ParkReason::LedgerUnavailable),
                TurnDecision::Park(ParkReason::LedgerUnavailable),
            ),
            (
                "revoked",
                |ctx| ctx.principal_authorized = false,
                TurnDecision::Evict(EvictionReason::Revoked),
                TurnDecision::Evict(EvictionReason::Revoked),
            ),
            (
                "disabled",
                |ctx| ctx.selection_enabled = false,
                TurnDecision::Fallback(FallbackReason::SelectionDisabled),
                TurnDecision::Park(ParkReason::SelectionDisabled),
            ),
            (
                "exhausted",
                |ctx| ctx.group_budget_exhausted = true,
                TurnDecision::Fallback(FallbackReason::BudgetExhausted),
                TurnDecision::Evict(EvictionReason::BudgetExhausted),
            ),
            (
                "build_budget",
                |ctx| ctx.build_budget_available = false,
                TurnDecision::Defer(DeferReason::BuildBudget),
                TurnDecision::Defer(DeferReason::BuildBudget),
            ),
        ]
    }

    #[test]
    fn every_pairwise_combination_follows_precedence() {
        let blockers = blockers();
        for (mode_index, mode) in
            [FallbackMode::DefaultSibling, FallbackMode::Drop].into_iter().enumerate()
        {
            for (i, (first_name, first, default_decision, drop_decision)) in
                blockers.iter().enumerate()
            {
                let expected = if mode_index == 0 { *default_decision } else { *drop_decision };
                for (second_name, second, _, _) in &blockers[i..] {
                    let mut ctx = ready(mode);
                    first(&mut ctx);
                    second(&mut ctx);
                    assert_eq!(
                        GroupLifecycle::before_evaluation(&ctx),
                        expected,
                        "{first_name} + {second_name} in {}",
                        mode.as_label()
                    );
                }
            }
        }
    }

    #[test]
    fn evaluation_result_follows_step_eight() {
        use FallbackMode::{DefaultSibling, Drop};
        assert_eq!(GroupLifecycle::after_evaluation(true, Drop, false), TurnDecision::CommitWinner);
        assert_eq!(
            GroupLifecycle::after_evaluation(false, DefaultSibling, false),
            TurnDecision::Fallback(FallbackReason::NoEligibleSibling)
        );
        assert_eq!(
            GroupLifecycle::after_evaluation(false, Drop, true),
            TurnDecision::Park(ParkReason::NoEligibleSibling)
        );
        assert_eq!(
            GroupLifecycle::after_evaluation(false, Drop, false),
            TurnDecision::Evict(EvictionReason::BudgetExhausted)
        );
    }
}
