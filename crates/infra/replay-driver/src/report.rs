use alloy_primitives::U256;
use serde::Serialize;

use crate::{BlockDiff, DiffError};

/// Additive measurements; optional totals are unknown if any scored block is unknown.
#[derive(Debug, Default, Clone, Serialize)]
pub struct DiffTotals {
    /// Canonical hashes absent from candidate (includes recording gaps).
    pub missing: usize,
    /// Missing minus recording gaps; None when recording coverage is unknown.
    pub builder_missing: Option<usize>,
    /// Candidate hashes absent from canonical.
    pub extra: usize,
    /// Hashes present in both.
    pub common: usize,
    /// Missing hashes absent from recording; recording gap, not builder failure. None is unknown.
    pub recording_gap: Option<usize>,
    /// Canonical execution gas.
    pub canonical_gas_used: u128,
    /// Candidate execution gas.
    pub candidate_gas_used: u128,
    /// Canonical execution tips in wei, not total coinbase balance changes.
    pub canonical_priority_fee_wei: Option<U256>,
    /// Builder `block_value` in wei.
    pub candidate_priority_fee_wei: U256,
    /// Build-to-getPayload elapsed microseconds, including wait (sum for aggregate).
    pub build_latency_us: u128,
    /// Observed flashblocks (sum for aggregate); None until subscription is available.
    pub flashblock_count: Option<u64>,
}

impl<'a> std::iter::Sum<&'a Self> for DiffTotals {
    fn sum<I: Iterator<Item = &'a Self>>(iter: I) -> Self {
        iter.fold(
            Self {
                recording_gap: Some(0),
                builder_missing: Some(0),
                canonical_priority_fee_wei: Some(U256::ZERO),
                flashblock_count: Some(0),
                ..Self::default()
            },
            |a, b| Self {
                missing: a.missing + b.missing,
                builder_missing: a.builder_missing.zip(b.builder_missing).map(|(a, b)| a + b),
                extra: a.extra + b.extra,
                common: a.common + b.common,
                recording_gap: a.recording_gap.zip(b.recording_gap).map(|(a, b)| a + b),
                canonical_gas_used: a.canonical_gas_used + b.canonical_gas_used,
                candidate_gas_used: a.candidate_gas_used + b.candidate_gas_used,
                canonical_priority_fee_wei: a
                    .canonical_priority_fee_wei
                    .zip(b.canonical_priority_fee_wei)
                    .map(|(a, b)| a + b),
                candidate_priority_fee_wei: a.candidate_priority_fee_wei
                    + b.candidate_priority_fee_wei,
                build_latency_us: a.build_latency_us + b.build_latency_us,
                flashblock_count: a.flashblock_count.zip(b.flashblock_count).map(|(a, b)| a + b),
            },
        )
    }
}

/// Versioned serde JSON report for N+1..M; no fidelity threshold is imposed.
#[derive(Debug, Serialize)]
pub struct DiffReport {
    /// Stable wire schema identifier.
    pub schema: &'static str,
    /// N: canonical parent of the first replayed block.
    pub parent_block_number: u64,
    /// Ordered per-block results, including warmup.
    pub blocks: Vec<BlockDiff>,
    /// Totals excluding N+1; empty scored windows have zero totals.
    pub aggregate: DiffTotals,
}

impl DiffReport {
    /// Marks N+1 warmup and sums only subsequent blocks.
    ///
    /// # Errors
    /// Returns [`DiffError::Window`] for gaps, duplicates, reversed or overflowing heights.
    pub fn new(parent_block_number: u64, mut blocks: Vec<BlockDiff>) -> Result<Self, DiffError> {
        let mut previous = parent_block_number;
        for (index, block) in blocks.iter_mut().enumerate() {
            if previous.checked_add(1) != Some(block.block_number) {
                return Err(DiffError::Window);
            }
            block.warmup = index == 0;
            previous = block.block_number;
        }
        let aggregate = blocks.iter().filter(|b| !b.warmup).map(|b| &b.totals).sum();
        Ok(Self { schema: "replay-diff/v1", parent_block_number, blocks, aggregate })
    }

    /// Renders inclusion and ordering summaries; unknown recording gaps are explicit.
    pub fn markdown(&self) -> String {
        let mut out = String::from(
            "# Replay diff\n\nN+1 is warmup; aggregate excludes it. Gaps are missing transactions absent from recording, not builder failures. Builder-attributable missing excludes recording gaps and is unknown when recording coverage is unknown. Resource measurements are in JSON; fees are execution tips (wei), latency is summed microseconds.\n\n| Block | Missing (incl. recording gaps) | Builder-attributable missing | Extra | Common | Recording gap | First divergence |\n| --- | --- | --- | --- | --- | --- | --- |\n",
        );
        let rows = self
            .blocks
            .iter()
            .map(|b| {
                (
                    format!("{}{}", b.block_number, if b.warmup { " (warmup)" } else { "" }),
                    &b.totals,
                    b.first_divergence_index,
                )
            })
            .chain(std::iter::once(("Aggregate".into(), &self.aggregate, None)));
        for (label, t, divergence) in rows {
            out.push_str(&format!(
                "| {label} | {} | {} | {} | {} | {} | {} |\n",
                t.missing,
                t.builder_missing.map_or_else(|| "unknown".into(), |v| v.to_string()),
                t.extra,
                t.common,
                t.recording_gap.map_or_else(|| "unknown".into(), |v| v.to_string()),
                divergence.map_or_else(|| "—".into(), |v| v.to_string())
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, time::Duration};

    use alloy_primitives::B256;

    use super::*;
    use crate::{Candidate, CanonicalBlock};

    fn block(block_number: u64, totals: DiffTotals) -> BlockDiff {
        BlockDiff { block_number, warmup: false, first_divergence_index: None, totals }
    }

    #[test]
    fn sums_each_optional_measurement_when_known_and_unknown_values_mix() {
        let known = DiffTotals {
            missing: 5,
            builder_missing: Some(3),
            extra: 2,
            common: 3,
            recording_gap: Some(2),
            canonical_gas_used: 11,
            candidate_gas_used: 7,
            canonical_priority_fee_wei: Some(U256::from(17)),
            candidate_priority_fee_wei: U256::from(13),
            build_latency_us: 19,
            flashblock_count: Some(23),
        };
        for unknown in [
            known.clone(),
            DiffTotals { recording_gap: None, builder_missing: None, ..known.clone() },
            DiffTotals { canonical_priority_fee_wei: None, ..known.clone() },
            DiffTotals { flashblock_count: None, ..known.clone() },
        ] {
            for inputs in [[&known, &unknown], [&unknown, &known]] {
                let sum: DiffTotals = inputs.into_iter().sum();

                assert_eq!((sum.missing, sum.extra, sum.common), (10, 4, 6));
                assert_eq!((sum.canonical_gas_used, sum.candidate_gas_used), (22, 14));
                assert_eq!(sum.candidate_priority_fee_wei, U256::from(26));
                assert_eq!(sum.build_latency_us, 38);
                assert_eq!(sum.recording_gap, unknown.recording_gap.map(|_| 4));
                assert_eq!(sum.builder_missing, unknown.builder_missing.map(|_| 6));
                assert_eq!(
                    sum.canonical_priority_fee_wei,
                    unknown.canonical_priority_fee_wei.map(|_| U256::from(34))
                );
                assert_eq!(sum.flashblock_count, unknown.flashblock_count.map(|_| 46));
            }
        }
    }

    #[test]
    fn excludes_unknown_measurements_when_block_is_warmup() {
        let scored = DiffTotals {
            missing: 3,
            builder_missing: Some(2),
            recording_gap: Some(1),
            canonical_priority_fee_wei: Some(U256::from(7)),
            flashblock_count: Some(2),
            ..DiffTotals::default()
        };
        let blocks = vec![
            block(11, DiffTotals { missing: 99, ..DiffTotals::default() }),
            block(12, scored.clone()),
        ];

        let report = DiffReport::new(10, blocks).unwrap();

        assert!(report.blocks[0].warmup);
        assert!(!report.blocks[1].warmup);
        assert_eq!(
            serde_json::to_value(&report.aggregate).unwrap(),
            serde_json::to_value(scored).unwrap()
        );
    }

    #[test]
    fn yields_known_zero_totals_when_report_is_empty_or_warmup_only() {
        for blocks in [vec![], vec![block(11, DiffTotals { missing: 99, ..DiffTotals::default() })]]
        {
            let report = DiffReport::new(10, blocks).unwrap();

            let totals = &report.aggregate;
            assert_eq!((totals.missing, totals.extra, totals.common), (0, 0, 0));
            assert_eq!(
                (totals.canonical_gas_used, totals.candidate_gas_used, totals.build_latency_us),
                (0, 0, 0)
            );
            assert_eq!(totals.candidate_priority_fee_wei, U256::ZERO);
            assert_eq!(totals.recording_gap, Some(0));
            assert_eq!(totals.builder_missing, Some(0));
            assert_eq!(totals.canonical_priority_fee_wei, Some(U256::ZERO));
            assert_eq!(totals.flashblock_count, Some(0));
        }
    }

    #[test]
    fn rejects_window_when_heights_are_not_contiguous_after_parent() {
        for (parent, heights) in [
            (10, vec![12]),
            (10, vec![11, 13]),
            (10, vec![11, 11]),
            (10, vec![11, 10]),
            (u64::MAX, vec![0]),
        ] {
            let blocks = heights.into_iter().map(|n| block(n, DiffTotals::default())).collect();

            let result = DiffReport::new(parent, blocks);

            assert!(matches!(result, Err(DiffError::Window)));
        }
    }

    #[test]
    fn preserves_json_contract_when_measurements_are_known_or_unknown() {
        let mut blocks = Vec::new();
        for number in 11..=12 {
            let candidate = Candidate {
                block_number: number,
                transaction_hashes: vec![B256::ZERO, B256::repeat_byte(3)],
                gas_used: 21,
                priority_fee_total: U256::from(42),
                build_latency: Duration::from_micros(9),
                flashblock_count: (number == 12).then_some(4),
            };
            let canonical = CanonicalBlock {
                block_number: number,
                transaction_hashes: vec![B256::ZERO, B256::repeat_byte(1), B256::repeat_byte(2)],
                gas_used: 63,
                priority_fee_total: (number == 12).then_some(U256::from(255)),
            };
            let recorded = HashSet::from([B256::repeat_byte(1)]);
            blocks.push(
                BlockDiff::compare(&candidate, &canonical, (number == 12).then_some(&recorded))
                    .unwrap(),
            );
        }

        let report = DiffReport::new(10, blocks).unwrap();

        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/diff-report-v1.json")).unwrap();
        assert_eq!(serde_json::to_value(&report).unwrap(), expected);
        let markdown = report.markdown();
        assert!(markdown.contains("| Missing (incl. recording gaps) |"));
        assert!(markdown.contains("| Builder-attributable missing |"));
        assert!(markdown.contains("| 11 (warmup) | 2 | unknown | 1 | 1 | unknown | 1 |"));
        assert!(markdown.contains("| Aggregate | 2 | 1 | 1 | 1 | 1 | — |"));
    }
}
