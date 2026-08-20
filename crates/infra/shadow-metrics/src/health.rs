//! Release-health checks for a shadow block, compared to the canonical block
//! that replaced it. Authoritative here (not the UI) so the same pass/fail
//! definitions back the explorer, metrics, and any future alerting.

use serde::Serialize;

use crate::ShadowBlockStats;

/// Gas-divergence tolerance: a shadow block whose gas is within this percentage
/// of canonical is considered healthy.
pub const HEALTH_GAS_DIFF_THRESHOLD_PCT: f64 = 50.0;

/// One pass/fail health check with a human-readable detail string.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheck {
    pub id: &'static str,
    pub label: &'static str,
    pub passed: bool,
    pub detail: String,
}

/// The health verdict for a shadow block. `reconciled` is false when the
/// canonical replacement is not yet persisted, in which case the comparison
/// checks cannot run and `checks` is empty.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShadowBlockHealth {
    pub reconciled: bool,
    pub passed: usize,
    pub total: usize,
    pub checks: Vec<HealthCheck>,
}

/// Number of checks evaluated once a shadow block is reconciled with canonical.
const CHECK_COUNT: usize = 4;

impl ShadowBlockHealth {
    #[must_use]
    pub fn evaluate(shadow: &ShadowBlockStats, canonical: Option<&ShadowBlockStats>) -> Self {
        let Some(canonical) = canonical else {
            return Self { reconciled: false, passed: 0, total: CHECK_COUNT, checks: Vec::new() };
        };

        let checks = vec![
            gas_check(shadow, canonical),
            tx_count_check(shadow, canonical),
            non_deposit_check(shadow, canonical),
            inversion_check(shadow),
        ];
        let passed = checks.iter().filter(|check| check.passed).count();
        let total = checks.len();

        Self { reconciled: true, passed, total, checks }
    }
}

fn gas_check(shadow: &ShadowBlockStats, canonical: &ShadowBlockStats) -> HealthCheck {
    let (passed, detail) = if canonical.gas_used == 0 {
        (shadow.gas_used == 0, format!("shadow {} vs canonical 0", shadow.gas_used))
    } else {
        let pct = (shadow.gas_used as f64 - canonical.gas_used as f64) / canonical.gas_used as f64
            * 100.0;
        (
            pct.abs() <= HEALTH_GAS_DIFF_THRESHOLD_PCT,
            format!("{pct:+.1}% (shadow {} vs canonical {})", shadow.gas_used, canonical.gas_used),
        )
    };
    HealthCheck { id: "gas_in_band", label: "Gas within ±50% of canonical", passed, detail }
}

fn tx_count_check(shadow: &ShadowBlockStats, canonical: &ShadowBlockStats) -> HealthCheck {
    let passed = shadow.transaction_count == canonical.transaction_count;
    HealthCheck {
        id: "tx_count_match",
        label: "Transaction count matches canonical",
        passed,
        detail: format!(
            "shadow {} vs canonical {}",
            shadow.transaction_count, canonical.transaction_count
        ),
    }
}

fn non_deposit_check(shadow: &ShadowBlockStats, canonical: &ShadowBlockStats) -> HealthCheck {
    let passed = shadow.non_deposit_tx_count == canonical.non_deposit_tx_count;
    HealthCheck {
        id: "non_deposit_tx_count_match",
        label: "Non-deposit transaction count matches canonical",
        passed,
        detail: format!(
            "shadow {} vs canonical {}",
            shadow.non_deposit_tx_count, canonical.non_deposit_tx_count
        ),
    }
}

fn inversion_check(shadow: &ShadowBlockStats) -> HealthCheck {
    let passed = shadow.priority_fee_inversions == 0;
    HealthCheck {
        id: "no_priority_fee_inversions",
        label: "No priority-fee inversions",
        passed,
        detail: format!("{} inversion(s)", shadow.priority_fee_inversions),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(gas_used: u64, tx: usize, non_deposit: usize, inversions: usize) -> ShadowBlockStats {
        ShadowBlockStats {
            number: 1,
            gas_used,
            transaction_count: tx,
            non_deposit_tx_count: non_deposit,
            priority_fee_inversions: inversions,
            builder_version: String::new(),
        }
    }

    #[test]
    fn all_checks_pass_when_shadow_matches_canonical() {
        let health =
            ShadowBlockHealth::evaluate(&stats(20_000, 2, 1, 0), Some(&stats(20_000, 2, 1, 0)));
        assert!(health.reconciled);
        assert_eq!(health.total, 4);
        assert_eq!(health.passed, 4);
    }

    #[test]
    fn out_of_band_gas_and_inversions_fail() {
        let health =
            ShadowBlockHealth::evaluate(&stats(60_000, 2, 1, 3), Some(&stats(20_000, 2, 1, 0)));
        assert_eq!(health.total, 4);
        assert_eq!(health.passed, 2);
        let gas = health.checks.iter().find(|c| c.id == "gas_in_band").unwrap();
        assert!(!gas.passed);
        let inversions =
            health.checks.iter().find(|c| c.id == "no_priority_fee_inversions").unwrap();
        assert!(!inversions.passed);
    }

    #[test]
    fn unreconciled_is_pending_with_no_checks() {
        let health = ShadowBlockHealth::evaluate(&stats(20_000, 2, 1, 0), None);
        assert!(!health.reconciled);
        assert_eq!(health.passed, 0);
        assert_eq!(health.total, 4);
        assert!(health.checks.is_empty());
    }
}
