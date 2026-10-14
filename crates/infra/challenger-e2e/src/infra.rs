//! Failures of what a run depends on, kept apart from failures of the challenger.
//!
//! A run reads canonical roots from the L2 RPC and picks its games from the
//! real chain. When either is unavailable the run cannot test anything, and a
//! failure then says nothing about the challenger. Such failures carry an
//! [`Unavailable`] marker: the binary logs them with `infra=true` and exits with
//! [`INFRA_EXIT_CODE`], so triage and the Workflow's retry policy can tell them
//! from a regression.

use std::{fmt, time::Duration};

use alloy_provider::Provider;
use base_common_consensus::Predeploys;
use base_proof_rpc::L2HttpProvider;
use eyre::{Result, WrapErr, eyre};
use tokio::time::Instant;
use tracing::warn;

/// Exit code of a run that failed because a dependency was unavailable.
///
/// Distinct from the generic `1` so the Workflow can retry these and only these.
pub const INFRA_EXIT_CODE: i32 = 2;

/// How often a retry loop logs while a dependency stays unavailable.
const RETRY_LOG_EVERY: Duration = Duration::from_secs(30);

/// Longest one read of a dependency may take before it counts as failed.
///
/// The RPC clients set no request timeout, and a node that accepts the
/// connection but never answers is a common way for one to fail. Without a
/// bound, such a read blocks the run past every budget and deadline it has.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Error messages of RPC failures that clear up on their own.
///
/// Matched case-insensitively against the whole error chain. Kept narrow on
/// purpose: an error that matches is retried and, if it persists, excused from
/// counting against the challenger.
const TRANSIENT_PATTERNS: [&str; 11] = [
    // reth while a node restarts or before it has state for a block.
    "no state found",
    "header not found",
    "missing trie node",
    // The request never got an answer.
    "error sending request",
    "connection refused",
    "connection reset",
    // An attempt cut off by `ATTEMPT_TIMEOUT`.
    "did not answer within",
    // A load balancer in front of the node.
    "502 bad gateway",
    "503 service unavailable",
    "504 gateway timeout",
    "429 too many requests",
];

/// Marks a failure as caused by a dependency of the run, not by the challenger.
#[derive(Debug)]
pub struct Unavailable(pub String);

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "infrastructure unavailable: {}", self.0)
    }
}

impl Unavailable {
    /// Whether `error` failed the run because a dependency was unavailable.
    pub fn is_infra(error: &eyre::Report) -> bool {
        error.downcast_ref::<Self>().is_some()
    }

    /// Adds the [`Unavailable`] marker to `error`.
    pub fn mark(error: eyre::Report, what: impl Into<String>) -> eyre::Report {
        error.wrap_err(Self(what.into()))
    }

    /// Whether `error` is an RPC failure that clears up on its own.
    pub fn looks_transient(error: &eyre::Report) -> bool {
        let rendered = format!("{error:#}").to_ascii_lowercase();
        TRANSIENT_PATTERNS.iter().any(|pattern| rendered.contains(pattern))
    }

    /// Marks `error` as [`Unavailable`] if it [`Self::looks_transient`].
    pub fn classify(error: eyre::Report, what: &str) -> eyre::Report {
        if Self::looks_transient(&error) { Self::mark(error, what) } else { error }
    }

    /// Runs `attempt` until it succeeds, retrying failures that [`Self::looks_transient`]
    /// for up to `budget`.
    ///
    /// A failure that persists past the budget comes back [`Unavailable`]; any
    /// other failure comes back at once and unmarked. Each attempt is cut off
    /// after [`ATTEMPT_TIMEOUT`] and counts as a transient failure, so the budget
    /// also bounds attempts that never return.
    pub async fn retry<T, F, Fut>(
        budget: Duration,
        interval: Duration,
        what: &str,
        mut attempt: F,
    ) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let started = Instant::now();
        let mut last_log: Option<Instant> = None;
        loop {
            let error = match tokio::time::timeout(ATTEMPT_TIMEOUT, attempt()).await {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(error)) => error,
                Err(_) => Self::no_answer(what),
            };
            if !Self::looks_transient(&error) {
                return Err(error);
            }
            let elapsed = started.elapsed();
            if elapsed >= budget {
                return Err(Self::mark(error, format!("{what} for {}s", elapsed.as_secs())));
            }
            if last_log.is_none_or(|at| at.elapsed() >= RETRY_LOG_EVERY) {
                warn!(
                    what,
                    error = %error,
                    elapsed_s = elapsed.as_secs(),
                    budget_s = budget.as_secs(),
                    "{what} is unavailable; retrying"
                );
                last_log = Some(Instant::now());
            }
            tokio::time::sleep(interval.min(budget - elapsed)).await;
        }
    }

    /// Reads the state proof a canonical output root needs, at the L2 head.
    ///
    /// The same `eth_getProof` the driver and the challenger make for every root,
    /// so it fails the same way when the node has no state to serve.
    ///
    /// Cut off after [`ATTEMPT_TIMEOUT`], which counts as a transient failure.
    pub async fn probe_l2_state(l2: &L2HttpProvider) -> Result<()> {
        let probe = async {
            let head = l2.get_block_number().await.wrap_err("failed to read the L2 head")?;
            l2.get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Vec::new())
                .number(head)
                .await
                .wrap_err_with(|| format!("failed to read L2 state at the head, block {head}"))?;
            Ok(())
        };
        tokio::time::timeout(ATTEMPT_TIMEOUT, probe)
            .await
            .unwrap_or_else(|_| Err(Self::no_answer("the L2 RPC")))
    }

    /// The error for an attempt cut off by [`ATTEMPT_TIMEOUT`]; it
    /// [`Self::looks_transient`].
    pub fn no_answer(what: &str) -> eyre::Report {
        eyre!("{what} did not answer within {}s", ATTEMPT_TIMEOUT.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn reth_and_transport_failures_are_transient() {
        for message in [
            "server returned an error response: error code -32603: no state found for block \
             number 4788534",
            "error sending request for url (https://l2.example/)",
            "HTTP error 503 Service Unavailable",
        ] {
            let error = eyre::eyre!("{message}").wrap_err("failed to fetch proof");
            assert!(Unavailable::looks_transient(&error), "{message}");
        }
    }

    #[test]
    fn assertion_failures_are_not_transient() {
        let error = eyre::eyre!("timed out after 45m waiting for the challenger to dispute");
        assert!(!Unavailable::looks_transient(&error));
        assert!(!Unavailable::is_infra(&Unavailable::classify(error, "L2 RPC")));
    }

    #[test]
    fn the_marker_survives_further_context() {
        let error = Unavailable::mark(eyre::eyre!("no state found for block number 1"), "L2 RPC")
            .wrap_err("failed to corrupt an intermediate output root on the fork")
            .wrap_err("Path 1 failed");
        assert!(Unavailable::is_infra(&error));
        assert!(format!("{error:#}").contains("infrastructure unavailable: L2 RPC"));
    }

    #[tokio::test]
    async fn retry_recovers_from_a_transient_failure() {
        let calls = AtomicU32::new(0);
        let value = Unavailable::retry(
            Duration::from_secs(5),
            Duration::from_millis(1),
            "L2 RPC",
            || async {
                if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                    eyre::bail!("no state found for block number 7");
                }
                Ok(7)
            },
        )
        .await
        .expect("third attempt succeeds");
        assert_eq!(value, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_marks_a_failure_that_outlasts_the_budget() {
        let error = Unavailable::retry(
            Duration::from_millis(20),
            Duration::from_millis(5),
            "L2 RPC",
            || async { Err::<(), _>(eyre::eyre!("no state found for block number 7")) },
        )
        .await
        .expect_err("never succeeds");
        assert!(Unavailable::is_infra(&error), "{error:#}");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_cuts_off_an_attempt_that_never_answers() {
        let error =
            Unavailable::retry(ATTEMPT_TIMEOUT * 2, Duration::from_millis(5), "L2 RPC", || {
                std::future::pending::<Result<()>>()
            })
            .await
            .expect_err("never answers");
        assert!(Unavailable::is_infra(&error), "{error:#}");
        assert!(format!("{error:#}").contains("did not answer within 30s"), "{error:#}");
    }

    #[tokio::test]
    async fn retry_returns_other_failures_at_once() {
        let calls = AtomicU32::new(0);
        let error = Unavailable::retry(
            Duration::from_secs(5),
            Duration::from_millis(1),
            "L2 RPC",
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(eyre::eyre!("account proof verification failed"))
            },
        )
        .await
        .expect_err("not retried");
        assert!(!Unavailable::is_infra(&error));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
