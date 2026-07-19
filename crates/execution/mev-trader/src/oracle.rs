//! Independent exact-prefix oracle types and classification.

use std::fmt::Debug;

use alloy_primitives::B256;

use crate::{PortError, SnapshotHandle, TransactionVisitor, VictimFrame, VisitSummary};

/// Comparable state digest produced by one oracle implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OracleDigest(pub B256);

/// Four mutually exclusive oracle classifications in precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OracleOutcome {
    /// At least one implementation could not produce a comparable result.
    NotComparable,
    /// The predecessor implementation disagreed with the independent implementation.
    ImplementationMismatch,
    /// Independent and predecessor results agree, but actual-prefix replay disagrees.
    InterveningStateDrift,
    /// All three implementations produced the same digest.
    Match,
}

/// Completed P/Q/R oracle observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleObservation {
    /// P: result produced from the frozen predecessor implementation.
    pub predecessor: Option<OracleDigest>,
    /// Q: result produced by the separate independent implementation.
    pub independent: Option<OracleDigest>,
    /// R: result produced after replaying the exact captured prefix.
    pub exact_prefix: Option<OracleDigest>,
}

impl OracleObservation {
    /// Classifies P/Q/R with fail-closed precedence.
    pub fn classify(self) -> OracleOutcome {
        let (Some(predecessor), Some(independent), Some(exact_prefix)) =
            (self.predecessor, self.independent, self.exact_prefix)
        else {
            return OracleOutcome::NotComparable;
        };
        if predecessor.0 != independent.0 {
            OracleOutcome::ImplementationMismatch
        } else if independent.0 != exact_prefix.0 {
            OracleOutcome::InterveningStateDrift
        } else {
            OracleOutcome::Match
        }
    }
}

/// Frozen predecessor result and captured transaction count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrozenPredecessor {
    /// P produced against the captured predecessor state.
    pub digest: Option<OracleDigest>,
    /// Exact pending transaction count captured with P.
    pub transaction_count: usize,
}

/// Dedicated predecessor implementation used only for P and exact-target selection.
pub trait PredecessorOracle: Debug {
    /// Produces P against the captured prestate and victim.
    fn freeze(
        &mut self,
        snapshot: &SnapshotHandle,
        victim: &VictimFrame,
    ) -> Result<Option<OracleDigest>, PortError>;

    /// Selects the exact replay target for the frozen transaction count.
    fn exact_target(
        &mut self,
        snapshot: &SnapshotHandle,
        victim: &VictimFrame,
        frozen_transaction_count: usize,
    ) -> Result<Option<usize>, PortError>;
}

/// Separate implementation used only for victim-only Q.
pub trait IndependentOracle: Debug {
    /// Produces Q against the same captured prestate and victim.
    fn victim_only(
        &mut self,
        snapshot: &SnapshotHandle,
        victim: &VictimFrame,
    ) -> Result<Option<OracleDigest>, PortError>;
}

/// Actual-prefix implementation used only for replay and R.
pub trait ExactPrefixOracle: TransactionVisitor {
    /// Starts exact-prefix replay for the selected target and victim.
    fn begin(
        &mut self,
        snapshot: &SnapshotHandle,
        victim: &VictimFrame,
        target: usize,
    ) -> Result<(), PortError>;

    /// Completes victim execution after exact-prefix replay and produces R.
    fn finish(
        &mut self,
        snapshot: &SnapshotHandle,
        victim: &VictimFrame,
    ) -> Result<Option<OracleDigest>, PortError>;
}

/// Result of one six-step independent exact-prefix evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleEvaluation {
    /// Frozen predecessor/count pair from step one.
    pub frozen: FrozenPredecessor,
    /// Complete P/Q/R observations from step five.
    pub observation: OracleObservation,
    /// Exclusive classification from step six.
    pub outcome: OracleOutcome,
}

/// Runs the fixed six-step oracle without suffix replay or hot-path waiting.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExactPrefixCoordinator;

impl ExactPrefixCoordinator {
    /// Runs predecessor/count freeze, exact target, prefix replay, victim-only, completion, classify.
    pub fn evaluate(
        &self,
        snapshot: &SnapshotHandle,
        victim: &VictimFrame,
        predecessor: &mut dyn PredecessorOracle,
        independent: &mut dyn IndependentOracle,
        exact_prefix: &mut dyn ExactPrefixOracle,
    ) -> Result<OracleEvaluation, PortError> {
        let frozen = FrozenPredecessor {
            digest: predecessor.freeze(snapshot, victim)?,
            transaction_count: snapshot.latest_block_transaction_count(),
        };

        let target = predecessor.exact_target(snapshot, victim, frozen.transaction_count)?;
        let Some(target) = target.filter(|target| *target == frozen.transaction_count) else {
            return Ok(Self::not_comparable(frozen));
        };

        exact_prefix.begin(snapshot, victim, target)?;
        let summary = snapshot.visit_transactions_for_block(
            snapshot.latest_block_number(),
            0,
            target,
            exact_prefix,
        )?;
        if !Self::replayed_exact_target(summary, target)? {
            return Ok(Self::not_comparable(frozen));
        }
        let exact_prefix_digest = exact_prefix.finish(snapshot, victim)?;
        let independent_digest = independent.victim_only(snapshot, victim)?;

        let observation = OracleObservation {
            predecessor: frozen.digest,
            independent: independent_digest,
            exact_prefix: exact_prefix_digest,
        };
        Ok(OracleEvaluation { frozen, observation, outcome: observation.classify() })
    }

    /// Returns whether replay exhausted exactly the frozen target count.
    pub fn replayed_exact_target(summary: VisitSummary, target: usize) -> Result<bool, PortError> {
        let expected = u32::try_from(target).map_err(|_| PortError::LimitExceeded)?;
        Ok(summary.complete && summary.visited == expected)
    }

    /// Constructs a fail-closed evaluation before Q or R can be completed.
    pub const fn not_comparable(frozen: FrozenPredecessor) -> OracleEvaluation {
        let observation =
            OracleObservation { predecessor: frozen.digest, independent: None, exact_prefix: None };
        OracleEvaluation { frozen, observation, outcome: OracleOutcome::NotComparable }
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc, time::Instant};

    use alloy_consensus::{Header, Sealed};
    use alloy_eips::Decodable2718;
    use alloy_primitives::{Address, Bytes};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_consensus::BaseTxEnvelope;
    use revm_bytecode::Bytecode;
    use revm_database::BundleAccount;

    use super::*;
    use crate::{
        BundleVisitor, PayloadVisitor, PendingSnapshotView, SnapshotHandleFactory, VisitControl,
    };

    const TX: &str = "02f86c0d010183072335825208940000000000000000000000000000000000000000872386f26fc1000080c001a0cdb9e4f2f1ba53f9429077e7055e078cf599786e29059cd80c5e0e923bb2c114a01c90e29201e031baf1da66296c3a5c15c200bcb5e6c34da2f05f7d1778f8be07";

    #[derive(Debug)]
    struct OracleView {
        transactions: Vec<BaseTxEnvelope>,
        frozen_count: usize,
    }

    impl PendingSnapshotView for OracleView {
        fn parent_hash(&self) -> B256 {
            B256::with_last_byte(1)
        }

        fn latest_block_number(&self) -> u64 {
            100
        }

        fn canonical_block_number(&self) -> u64 {
            99
        }

        fn latest_flashblock_index(&self) -> u64 {
            1
        }

        fn latest_header(&self) -> Sealed<Header> {
            Sealed::new_unchecked(Header::default(), B256::ZERO)
        }

        fn latest_block_transaction_count(&self) -> usize {
            self.frozen_count
        }

        fn has_transaction_hash(&self, _transaction_hash: B256) -> bool {
            false
        }

        fn transaction_position(
            &self,
            _block_number: u64,
            _transaction_hash: B256,
        ) -> Option<usize> {
            None
        }

        fn visit_latest_block_payloads(
            &self,
            _visitor: &mut dyn PayloadVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
        }

        fn visit_transactions_for_block(
            &self,
            _block_number: u64,
            start: usize,
            limit: usize,
            visitor: &mut dyn TransactionVisitor,
        ) -> Result<VisitSummary, PortError> {
            let mut visited = 0;
            for (position, transaction) in
                self.transactions.iter().enumerate().skip(start).take(limit)
            {
                visited += 1;
                if visitor.visit(position, transaction)? == crate::VisitControl::Stop {
                    return Ok(VisitSummary { visited, complete: false });
                }
            }
            Ok(VisitSummary {
                visited,
                complete: start.saturating_add(limit) >= self.transactions.len(),
            })
        }

        fn visit_bundle(
            &self,
            _visitor: &mut dyn BundleVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
        }
    }

    #[derive(Debug)]
    struct Pred {
        digest: Option<OracleDigest>,
        target: Option<usize>,
    }

    impl PredecessorOracle for Pred {
        fn freeze(
            &mut self,
            _snapshot: &SnapshotHandle,
            _victim: &VictimFrame,
        ) -> Result<Option<OracleDigest>, PortError> {
            Ok(self.digest)
        }

        fn exact_target(
            &mut self,
            _snapshot: &SnapshotHandle,
            _victim: &VictimFrame,
            _frozen_transaction_count: usize,
        ) -> Result<Option<usize>, PortError> {
            Ok(self.target)
        }
    }

    #[derive(Debug)]
    struct Independent(Option<OracleDigest>);

    impl IndependentOracle for Independent {
        fn victim_only(
            &mut self,
            _snapshot: &SnapshotHandle,
            _victim: &VictimFrame,
        ) -> Result<Option<OracleDigest>, PortError> {
            Ok(self.0)
        }
    }

    #[derive(Debug)]
    struct Prefix {
        digest: Option<OracleDigest>,
        visited: usize,
        begun: bool,
    }

    impl TransactionVisitor for Prefix {
        fn visit(
            &mut self,
            _position: usize,
            _transaction: &BaseTxEnvelope,
        ) -> Result<VisitControl, PortError> {
            self.visited += 1;
            Ok(VisitControl::Continue)
        }
    }

    impl ExactPrefixOracle for Prefix {
        fn begin(
            &mut self,
            _snapshot: &SnapshotHandle,
            _victim: &VictimFrame,
            _target: usize,
        ) -> Result<(), PortError> {
            self.begun = true;
            Ok(())
        }

        fn finish(
            &mut self,
            _snapshot: &SnapshotHandle,
            _victim: &VictimFrame,
        ) -> Result<Option<OracleDigest>, PortError> {
            Ok(self.digest)
        }
    }

    fn digest(byte: u8) -> OracleDigest {
        OracleDigest(B256::with_last_byte(byte))
    }

    fn snapshot(transaction_count: usize) -> SnapshotHandle {
        let transaction = BaseTxEnvelope::decode_2718_exact(
            Bytes::from_str(&format!("0x{TX}")).expect("bytes").as_ref(),
        )
        .expect("transaction");
        let view: Arc<dyn PendingSnapshotView + Send + Sync> = Arc::new(OracleView {
            transactions: vec![transaction; transaction_count],
            frozen_count: transaction_count,
        });
        SnapshotHandleFactory::new().issue(view, Instant::now()).expect("handle")
    }

    fn victim(now: Instant) -> VictimFrame {
        VictimFrame {
            chain_id: 13,
            transaction_type: 2,
            transaction_hash: B256::with_last_byte(9),
            from: Address::ZERO,
            raw_tx: Bytes::new(),
            parent_hash: B256::with_last_byte(1),
            block_number: 100,
            victim_flashblock_index: 2,
            received_at: now,
        }
    }

    #[test]
    fn classification_has_four_exclusive_precedence_outcomes() {
        let a = Some(digest(1));
        let b = Some(digest(2));
        assert_eq!(
            OracleObservation { predecessor: None, independent: a, exact_prefix: a }.classify(),
            OracleOutcome::NotComparable
        );
        assert_eq!(
            OracleObservation { predecessor: a, independent: b, exact_prefix: a }.classify(),
            OracleOutcome::ImplementationMismatch
        );
        assert_eq!(
            OracleObservation { predecessor: a, independent: a, exact_prefix: b }.classify(),
            OracleOutcome::InterveningStateDrift
        );
        assert_eq!(
            OracleObservation { predecessor: a, independent: a, exact_prefix: a }.classify(),
            OracleOutcome::Match
        );
    }

    #[test]
    fn six_steps_replay_exact_prefix_once() {
        let snapshot = snapshot(2);
        let victim = victim(Instant::now());
        let mut predecessor = Pred { digest: Some(digest(1)), target: Some(2) };
        let mut independent = Independent(Some(digest(1)));
        let mut prefix = Prefix { digest: Some(digest(1)), visited: 0, begun: false };

        let evaluation = ExactPrefixCoordinator
            .evaluate(&snapshot, &victim, &mut predecessor, &mut independent, &mut prefix)
            .expect("evaluation");

        assert_eq!(evaluation.outcome, OracleOutcome::Match);
        assert!(prefix.begun);
        assert_eq!(prefix.visited, 2);
    }

    #[test]
    fn target_mismatch_fails_before_prefix_or_independent_work() {
        let snapshot = snapshot(2);
        let victim = victim(Instant::now());
        let mut predecessor = Pred { digest: Some(digest(1)), target: Some(1) };
        let mut independent = Independent(Some(digest(1)));
        let mut prefix = Prefix { digest: Some(digest(1)), visited: 0, begun: false };

        let evaluation = ExactPrefixCoordinator
            .evaluate(&snapshot, &victim, &mut predecessor, &mut independent, &mut prefix)
            .expect("evaluation");

        assert_eq!(evaluation.outcome, OracleOutcome::NotComparable);
        assert!(!prefix.begun);
        assert_eq!(prefix.visited, 0);
    }

    #[test]
    fn visitor_type_dependencies_remain_borrowed() {
        fn shape(_account: &BundleAccount, _bytecode: &Bytecode) {}
        let _ = (shape, PayloadId::default());
    }
}
