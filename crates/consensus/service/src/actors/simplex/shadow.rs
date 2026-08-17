//! Read-only shadow comparator for the simplex rollout.
//!
//! The first step of Phase 3 (sequencer integration): observe the simplex
//! leadership signal ([`ConsensusStatus`]) and op-conductor's leadership
//! ([`Conductor::leader`]) side by side, emit divergence metrics, and log
//! mismatches — while **acting on nothing**. No proposes, no commits, no
//! forkchoice, and no redirect of any op-conductor authority; op-conductor stays
//! fully authoritative.
//!
//! This deliberately mirrors the shadow-sequencer precedent's discipline
//! (side-effect-free observation gated by a mode) but at the leadership layer.
//! The comparator is constructed only when `simplex_mode` is `Shadow` or higher,
//! and runs in the `non_fatal` spawn group so a comparison failure has zero blast
//! radius on the live path. It needs no commonware, so it is not feature-gated.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{ConsensusStatus, SimplexError, SimplexMode};
use crate::{Metrics, NodeActor, actors::sequencer::Conductor};

/// Outcome of a single read-only comparison between the simplex leadership signal
/// and op-conductor's leadership for this node.
///
/// Carries no side effects — [`ShadowComparator::record`] turns it into metrics
/// and logs — so the comparison logic is unit-testable without the global metrics
/// facade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOutcome {
    /// Both systems agree on whether this node is the leader.
    Agree,
    /// The two systems disagree on this node's leadership.
    Diverged {
        /// simplex's view of this node's leadership.
        simplex_leader: bool,
        /// op-conductor's view of this node's leadership.
        conductor_leader: bool,
    },
    /// op-conductor could not be queried (RPC error); no comparison was made.
    ConductorUnavailable,
}

/// Read-only comparator that observes simplex vs op-conductor leadership.
///
/// **Acts on nothing.** It samples the simplex [`ConsensusStatus`] watch, queries
/// op-conductor's [`Conductor::leader`], and emits a divergence counter + an
/// agreement gauge + structured logs. It never proposes, commits, or redirects
/// any op-conductor authority — the safe first step of the staged rollout.
///
/// Runs only in `Shadow` (and higher, once authoritative wiring lands); in
/// `Off`/`Passive` it is not constructed. If op-conductor is not configured there
/// is nothing to compare, so it idles until shutdown.
#[derive(Debug)]
pub struct ShadowComparator {
    mode: SimplexMode,
    status_rx: watch::Receiver<ConsensusStatus>,
    conductor: Option<Arc<dyn Conductor>>,
}

impl ShadowComparator {
    /// Creates a new read-only shadow comparator over the simplex status watch and
    /// (optionally) an op-conductor leadership source.
    pub const fn new(
        mode: SimplexMode,
        status_rx: watch::Receiver<ConsensusStatus>,
        conductor: Option<Arc<dyn Conductor>>,
    ) -> Self {
        Self { mode, status_rx, conductor }
    }

    /// Compares one simplex `status` sample against op-conductor's leadership,
    /// **without acting**. Side effects (metrics/logs) are applied separately by
    /// [`record`](Self::record), keeping this pure enough to unit-test with a mock
    /// conductor.
    async fn compare_once(
        conductor: &dyn Conductor,
        status: &ConsensusStatus,
    ) -> ComparisonOutcome {
        match conductor.leader().await {
            Ok(conductor_leader) if conductor_leader == status.is_leader => {
                ComparisonOutcome::Agree
            }
            Ok(conductor_leader) => {
                ComparisonOutcome::Diverged { simplex_leader: status.is_leader, conductor_leader }
            }
            Err(error) => {
                warn!(target: "simplex", error = %error, "shadow comparator: op-conductor leader() query failed");
                ComparisonOutcome::ConductorUnavailable
            }
        }
    }

    /// Emits metrics and structured logs for a comparison `outcome`. Read-only.
    fn record(outcome: ComparisonOutcome, view: u64) {
        match outcome {
            ComparisonOutcome::Agree => {
                Metrics::simplex_conductor_agreement().set(1.0);
                debug!(target: "simplex", view, "shadow comparator: simplex and op-conductor agree on leadership");
            }
            ComparisonOutcome::Diverged { simplex_leader, conductor_leader } => {
                Metrics::simplex_conductor_agreement().set(0.0);
                Metrics::leadership_divergence_total("simplex_vs_conductor").increment(1);
                warn!(
                    target: "simplex",
                    view,
                    simplex_leader,
                    conductor_leader,
                    "shadow comparator: leadership divergence between simplex and op-conductor"
                );
            }
            // Already logged in compare_once; no comparison was made, so nothing
            // to record on the agreement gauge or divergence counter.
            ComparisonOutcome::ConductorUnavailable => {}
        }
    }
}

#[async_trait]
impl NodeActor for ShadowComparator {
    type Error = SimplexError;
    type StartData = CancellationToken;

    /// Runs the read-only comparison loop until cancellation or until the simplex
    /// status channel closes. Compares on each simplex status change; acts on
    /// nothing. Isolation is the framework's job (the `non_fatal` spawn group), so
    /// returning `Ok`/`Err` here can never cancel the node.
    async fn start(mut self, cancellation: Self::StartData) -> Result<(), Self::Error> {
        let Some(conductor) = self.conductor.clone() else {
            info!(target: "simplex", mode = ?self.mode, "shadow comparator: no op-conductor configured; nothing to compare, idling until shutdown");
            cancellation.cancelled().await;
            return Ok(());
        };
        info!(target: "simplex", mode = ?self.mode, "shadow comparator started (read-only; acts on nothing)");

        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    info!(target: "simplex", "shadow comparator received shutdown signal");
                    return Ok(());
                }
                changed = self.status_rx.changed() => {
                    if changed.is_err() {
                        debug!(target: "simplex", "shadow comparator: simplex status channel closed; exiting (non-fatal)");
                        return Ok(());
                    }
                    let status = self.status_rx.borrow_and_update().clone();
                    let outcome = Self::compare_once(conductor.as_ref(), &status).await;
                    Self::record(outcome, status.view);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::actors::sequencer::{ConductorError, MockConductor};

    use super::*;

    fn status(is_leader: bool, view: u64) -> ConsensusStatus {
        ConsensusStatus { is_leader, finalized_head: None, view }
    }

    #[tokio::test]
    async fn compare_once_detects_agreement() {
        let mut conductor = MockConductor::new();
        conductor.expect_leader().once().returning(|| Ok(true));
        let outcome = ShadowComparator::compare_once(&conductor, &status(true, 3)).await;
        assert_eq!(outcome, ComparisonOutcome::Agree);
    }

    #[tokio::test]
    async fn compare_once_detects_divergence() {
        // simplex says this node leads; op-conductor says it does not.
        let mut conductor = MockConductor::new();
        conductor.expect_leader().once().returning(|| Ok(false));
        let outcome = ShadowComparator::compare_once(&conductor, &status(true, 9)).await;
        assert_eq!(
            outcome,
            ComparisonOutcome::Diverged { simplex_leader: true, conductor_leader: false }
        );
    }

    #[tokio::test]
    async fn compare_once_handles_conductor_error() {
        let mut conductor = MockConductor::new();
        conductor.expect_leader().once().returning(|| Err(ConductorError::NotLeader));
        let outcome = ShadowComparator::compare_once(&conductor, &status(false, 1)).await;
        assert_eq!(outcome, ComparisonOutcome::ConductorUnavailable);
    }

    /// With no op-conductor configured there is nothing to compare, so the actor
    /// idles until cancellation and returns `Ok` — without ever querying anything.
    #[tokio::test]
    async fn start_idles_when_no_conductor() {
        let (_status_tx, status_rx) = watch::channel(ConsensusStatus::default());
        let comparator = ShadowComparator::new(SimplexMode::Shadow, status_rx, None);
        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(comparator.start(cancellation.clone()));
        cancellation.cancel();
        assert!(handle.await.unwrap().is_ok());
    }

    /// A simplex status change drives exactly one read-only comparison (one
    /// `leader()` query), then cancellation ends the loop cleanly. Uses a call
    /// counter rather than mock-drop verification so the assertion is timing-robust.
    #[tokio::test]
    async fn start_compares_on_status_change() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in_mock = Arc::clone(&calls);
        let mut conductor = MockConductor::new();
        conductor.expect_leader().returning(move || {
            calls_in_mock.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        });

        let (status_tx, status_rx) = watch::channel(ConsensusStatus::default());
        let comparator =
            ShadowComparator::new(SimplexMode::Shadow, status_rx, Some(Arc::new(conductor)));
        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(comparator.start(cancellation.clone()));

        // Publish a status change; the comparator should query op-conductor once.
        status_tx.send_replace(status(true, 1));
        // Yield until the comparison is observed (bounded so a hang fails the test).
        for _ in 0..100 {
            if calls.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(calls.load(Ordering::SeqCst) >= 1, "a status change must drive a leader() query");

        cancellation.cancel();
        assert!(handle.await.unwrap().is_ok());
    }
}
