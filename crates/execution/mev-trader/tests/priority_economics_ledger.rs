#![doc = "Exhaustive selected-route priority-economics terminal ledger tests."]
use std::num::NonZeroUsize;

use alloy_primitives::{Address, B256, U256, aliases::I512};
use base_mev_trader::{
    AdmissionStageV2, AdmissionTerminalV2, AttemptedAuthorityUnavailableReasonV2,
    AttemptedAuthorityUnavailableV2, AuthorityUnavailableV2, CanonicalL1FeeEvidenceV2,
    DiscoveryAuthorityUnavailableV2, DiscoveryUnavailableReasonV2, EconomicDispositionV1,
    PreShapeAuthorityUnavailableV2, PreShapeUnavailableReasonV2, PriorityEconomicsCountersV2,
    PriorityEconomicsLedgerErrorV2, PriorityEconomicsLedgerV2, PriorityEconomicsV2,
    PriorityEconomicsValidationErrorV2, SelectedRouteEvidenceV2,
};

fn digest(byte: u8) -> B256 {
    B256::repeat_byte(byte)
}

fn address(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

fn route() -> SelectedRouteEvidenceV2 {
    SelectedRouteEvidenceV2::new(
        digest(1),
        digest(2),
        [address(1), address(2)],
        [address(3), address(4), address(5)],
        [digest(3), digest(4)],
        [500, 3_000],
        [true, false],
        U256::from(1_000),
        digest(5),
        digest(6),
        digest(7),
        digest(8),
        digest(9),
        digest(10),
    )
    .unwrap()
}

fn counters(values: [u64; 8]) -> PriorityEconomicsCountersV2 {
    PriorityEconomicsCountersV2::new(
        values[0], values[1], values[2], values[3], values[4], values[5], values[6], values[7],
    )
    .unwrap()
}

fn assert_send_sync<T: Send + Sync>() {}

fn evaluated(
    ev: i64,
    optimism: Option<bool>,
) -> Result<PriorityEconomicsV2, PriorityEconomicsValidationErrorV2> {
    let total = u64::try_from(25_i64 - ev).unwrap();
    let l1_fee =
        CanonicalL1FeeEvidenceV2::new(100, 20, 80, 90, digest(11), U256::from(total - 10)).unwrap();
    PriorityEconomicsV2::evaluated_from_execution(
        route(),
        U256::from(1_100),
        U256::from(75),
        U256::from(25),
        21_000,
        U256::from(10),
        l1_fee,
        optimism,
        counters([1, 1, 1, 1, 1, 1, 1, 0]),
    )
}

#[test]
fn stages_are_exactly_ordered_through_economics_evaluated() {
    assert!(AdmissionStageV2::NotRun < AdmissionStageV2::PipelineStarted);
    assert!(AdmissionStageV2::PipelineStarted < AdmissionStageV2::CandidateDiscoveryAttempted);
    assert!(AdmissionStageV2::CandidateDiscoveryAttempted < AdmissionStageV2::CandidateSetBuilt);
    assert!(AdmissionStageV2::CandidateSetBuilt < AdmissionStageV2::RouteBound);
    assert!(AdmissionStageV2::RouteBound < AdmissionStageV2::ShapeBuilt);
    assert!(AdmissionStageV2::ShapeBuilt < AdmissionStageV2::AuthorityAttempted);
    assert!(AdmissionStageV2::AuthorityAttempted < AdmissionStageV2::EconomicsEvaluated);
    assert_send_sync::<PriorityEconomicsLedgerV2>();
}

#[test]
fn terminal_pipeline_not_run_has_zero_counters_and_null_evidence() {
    let record = PriorityEconomicsV2::pipeline_not_run();
    assert_eq!(record.terminal(), AdmissionTerminalV2::PipelineNotRun);
    assert_eq!(record.stage(), AdmissionStageV2::NotRun);
    assert_eq!(record.counters().values(), [0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(record.route(), None);
    assert_eq!(record.authority_unavailable_reason(), None);
    assert!(record.validate().is_ok());
    assert_eq!(record.project_v1().disposition(), EconomicDispositionV1::NotReached);
}

#[test]
fn terminal_no_route_requires_completed_discovery() {
    let record = PriorityEconomicsV2::no_route(counters([1, 1, 0, 0, 0, 0, 0, 0])).unwrap();
    assert_eq!(record.terminal(), AdmissionTerminalV2::NoRoute);
    assert_eq!(record.stage(), AdmissionStageV2::CandidateSetBuilt);
    assert_eq!(record.counters().values(), [1, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(record.route(), None);
    assert_eq!(record.authority_unavailable_reason(), None);
    assert_eq!(record.project_v1().disposition(), EconomicDispositionV1::NoRoute);
}

#[test]
fn terminal_gross_nonpositive_preserves_signed_zero() {
    let selected_route = route();
    let record = PriorityEconomicsV2::gross_nonpositive(
        selected_route.clone(),
        I512::try_from(0).unwrap(),
        counters([1, 1, 1, 1, 0, 0, 0, 0]),
    )
    .unwrap();
    assert_eq!(record.terminal(), AdmissionTerminalV2::GrossNonpositive);
    assert_eq!(record.stage(), AdmissionStageV2::RouteBound);
    assert_eq!(record.counters().values(), [1, 1, 1, 1, 0, 0, 0, 0]);
    assert_eq!(record.route(), Some(&selected_route));
    assert_eq!(record.authority_unavailable_reason(), None);
}

#[test]
fn terminal_discovery_unavailable_is_exact_attempted_shape() {
    let reason = AuthorityUnavailableV2::Discovery(
        DiscoveryAuthorityUnavailableV2::new(
            DiscoveryUnavailableReasonV2::CandidateDiscoveryFailed,
            digest(12),
        )
        .unwrap(),
    );
    let record = PriorityEconomicsV2::authority_unavailable(
        AdmissionStageV2::CandidateDiscoveryAttempted,
        reason.clone(),
        None,
        None,
        counters([1, 0, 0, 0, 0, 0, 0, 0]),
    )
    .unwrap();
    assert_eq!(record.terminal(), AdmissionTerminalV2::AuthorityUnavailable);
    assert_eq!(record.stage(), AdmissionStageV2::CandidateDiscoveryAttempted);
    assert_eq!(record.counters().values(), [1, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(record.route(), None);
    assert_eq!(record.authority_unavailable_reason(), Some(&reason));
}

#[test]
fn terminal_pre_shape_unavailable_has_route_without_authority_failure() {
    let reason = AuthorityUnavailableV2::PreShape(
        PreShapeAuthorityUnavailableV2::new(
            AdmissionStageV2::RouteBound,
            PreShapeUnavailableReasonV2::CalldataConstructionFailed,
            digest(13),
        )
        .unwrap(),
    );
    let selected_route = route();
    let record = PriorityEconomicsV2::authority_unavailable(
        AdmissionStageV2::RouteBound,
        reason.clone(),
        Some(selected_route.clone()),
        Some(I512::try_from(100).unwrap()),
        counters([1, 1, 1, 1, 0, 0, 0, 0]),
    )
    .unwrap();
    assert_eq!(record.stage(), AdmissionStageV2::RouteBound);
    assert_eq!(record.counters().values(), [1, 1, 1, 1, 0, 0, 0, 0]);
    assert_eq!(record.route(), Some(&selected_route));
    assert_eq!(record.authority_unavailable_reason(), Some(&reason));
    assert!(matches!(
        PreShapeAuthorityUnavailableV2::new(
            AdmissionStageV2::ShapeBuilt,
            PreShapeUnavailableReasonV2::CalldataConstructionFailed,
            digest(13),
        ),
        Err(PriorityEconomicsValidationErrorV2::StageMismatch)
    ));
}

#[test]
fn terminal_attempted_unavailable_preserves_only_prior_successes() {
    let reason = AuthorityUnavailableV2::Attempted(
        AttemptedAuthorityUnavailableV2::new(
            AttemptedAuthorityUnavailableReasonV2::L1FeeUnavailable,
            digest(14),
            Some(U256::from(1_100)),
            Some(21_000),
            Some(U256::from(10)),
            None,
        )
        .unwrap(),
    );
    let selected_route = route();
    let record = PriorityEconomicsV2::authority_unavailable(
        AdmissionStageV2::AuthorityAttempted,
        reason.clone(),
        Some(selected_route.clone()),
        Some(I512::try_from(100).unwrap()),
        counters([1, 1, 1, 1, 1, 1, 0, 1]),
    )
    .unwrap();
    assert_eq!(record.stage(), AdmissionStageV2::AuthorityAttempted);
    assert_eq!(record.counters().values(), [1, 1, 1, 1, 1, 1, 0, 1]);
    assert_eq!(record.route(), Some(&selected_route));
    assert_eq!(record.authority_unavailable_reason(), Some(&reason));
}

#[test]
fn signed_ev_zero_is_selected_route_no_edge() {
    let record = evaluated(0, None).unwrap();
    assert_eq!(record.terminal(), AdmissionTerminalV2::SelectedRouteNoEdge);
    assert_eq!(record.ev_wei(), Some(I512::try_from(0).unwrap()));
    assert_eq!(record.counters().values(), [1, 1, 1, 1, 1, 1, 1, 0]);
    let ledger = PriorityEconomicsLedgerV2::new(NonZeroUsize::new(1).unwrap());
    ledger.append(record.clone()).unwrap();
    assert_eq!(ledger.snapshot().unwrap(), vec![record.clone()]);
    assert_eq!(
        evaluated(0, Some(true)),
        Err(PriorityEconomicsValidationErrorV2::ArithmeticMismatch)
    );
    assert_eq!(
        evaluated(0, Some(false)),
        Err(PriorityEconomicsValidationErrorV2::ArithmeticMismatch)
    );
    assert!(serde_json::to_value(&record).unwrap()["grossOptimismUnverified"].is_null());
}

#[test]
fn signed_ev_negative_is_selected_route_no_edge_with_shortfall() {
    let record = evaluated(-10, None).unwrap();
    assert_eq!(record.terminal(), AdmissionTerminalV2::SelectedRouteNoEdge);
    assert_eq!(record.project_v1().ev_wei_signed(), Some("-10"));
    assert_eq!(record.counters().values(), [1, 1, 1, 1, 1, 1, 1, 0]);

    let l1_fee = CanonicalL1FeeEvidenceV2::new(100, 20, 80, 90, digest(11), U256::from(5)).unwrap();
    assert_eq!(
        PriorityEconomicsV2::evaluated_from_execution(
            route(),
            U256::from(1_100),
            U256::from(74),
            U256::from(25),
            21_000,
            U256::from(10),
            l1_fee,
            None,
            counters([1, 1, 1, 1, 1, 1, 1, 0]),
        ),
        Err(PriorityEconomicsValidationErrorV2::ArithmeticMismatch)
    );
}

#[test]
fn signed_ev_positive_is_synthetic_only_and_not_net_ranked() {
    let record = evaluated(10, Some(true)).unwrap();
    assert_eq!(record.terminal(), AdmissionTerminalV2::SelectedRouteEvPositive);
    assert!(!record.net_ranked());
    assert_eq!(record.synthetic_reachability_only(), Some(true));
    assert_eq!(record.counters().values(), [1, 1, 1, 1, 1, 1, 1, 0]);

    let ledger = PriorityEconomicsLedgerV2::new(NonZeroUsize::new(1).unwrap());
    ledger.append(record.clone()).unwrap();
    assert_eq!(ledger.capacity(), NonZeroUsize::new(1).unwrap());
    assert_eq!(ledger.snapshot().unwrap(), vec![record]);
}

#[test]
fn validation_rejects_counter_conservation_mutant() {
    let arithmetic_conservation_mutant = PriorityEconomicsCountersV2::new(1, 1, 1, 1, 1, 2, 0, 1);
    assert_eq!(
        arithmetic_conservation_mutant,
        Err(PriorityEconomicsValidationErrorV2::CounterMismatch)
    );

    let no_route_extra_candidate_mutant =
        PriorityEconomicsV2::no_route(counters([1, 1, 1, 0, 0, 0, 0, 0]));
    assert_eq!(
        no_route_extra_candidate_mutant,
        Err(PriorityEconomicsValidationErrorV2::CounterMismatch)
    );

    let gross_shape_attempt_mutant = PriorityEconomicsV2::gross_nonpositive(
        route(),
        I512::try_from(0).unwrap(),
        counters([1, 1, 1, 1, 1, 0, 0, 0]),
    );
    assert_eq!(
        gross_shape_attempt_mutant,
        Err(PriorityEconomicsValidationErrorV2::CounterMismatch)
    );

    let discovery_success_mutant = PriorityEconomicsV2::authority_unavailable(
        AdmissionStageV2::CandidateDiscoveryAttempted,
        AuthorityUnavailableV2::Discovery(
            DiscoveryAuthorityUnavailableV2::new(
                DiscoveryUnavailableReasonV2::CandidateDiscoveryFailed,
                digest(16),
            )
            .unwrap(),
        ),
        None,
        None,
        counters([1, 1, 0, 0, 0, 0, 0, 0]),
    );
    assert_eq!(discovery_success_mutant, Err(PriorityEconomicsValidationErrorV2::CounterMismatch));

    let pre_shape_shape_attempt_mutant = PriorityEconomicsV2::authority_unavailable(
        AdmissionStageV2::RouteBound,
        AuthorityUnavailableV2::PreShape(
            PreShapeAuthorityUnavailableV2::new(
                AdmissionStageV2::RouteBound,
                PreShapeUnavailableReasonV2::CalldataConstructionFailed,
                digest(17),
            )
            .unwrap(),
        ),
        Some(route()),
        Some(I512::try_from(100).unwrap()),
        counters([1, 1, 1, 1, 1, 0, 0, 0]),
    );
    assert_eq!(
        pre_shape_shape_attempt_mutant,
        Err(PriorityEconomicsValidationErrorV2::CounterMismatch)
    );

    let attempted_multiple_failures_mutant = PriorityEconomicsV2::authority_unavailable(
        AdmissionStageV2::AuthorityAttempted,
        AuthorityUnavailableV2::Attempted(
            AttemptedAuthorityUnavailableV2::new(
                AttemptedAuthorityUnavailableReasonV2::L1FeeUnavailable,
                digest(18),
                Some(U256::from(1_100)),
                Some(21_000),
                Some(U256::from(10)),
                None,
            )
            .unwrap(),
        ),
        Some(route()),
        Some(I512::try_from(100).unwrap()),
        counters([1, 1, 1, 1, 1, 2, 0, 2]),
    );
    assert_eq!(
        attempted_multiple_failures_mutant,
        Err(PriorityEconomicsValidationErrorV2::CounterMismatch)
    );

    let evaluated_multiple_successes_mutant = PriorityEconomicsV2::evaluated_from_execution(
        route(),
        U256::from(1_100),
        U256::from(75),
        U256::from(25),
        21_000,
        U256::from(10),
        CanonicalL1FeeEvidenceV2::new(100, 20, 80, 90, digest(19), U256::from(15)).unwrap(),
        None,
        counters([1, 1, 1, 1, 1, 2, 2, 0]),
    );
    assert_eq!(
        evaluated_multiple_successes_mutant,
        Err(PriorityEconomicsValidationErrorV2::CounterMismatch)
    );

    let ledger = PriorityEconomicsLedgerV2::new(NonZeroUsize::new(1).unwrap());
    let rejected = no_route_extra_candidate_mutant.map_err(PriorityEconomicsLedgerErrorV2::from);
    assert_eq!(
        rejected,
        Err(PriorityEconomicsLedgerErrorV2::Validation(
            PriorityEconomicsValidationErrorV2::CounterMismatch,
        ))
    );
    assert!(ledger.snapshot().unwrap().is_empty());
}

#[test]
fn validation_rejects_l1_tuple_and_route_sentinels() {
    assert_eq!(
        CanonicalL1FeeEvidenceV2::new(100, 30, 60, 90, digest(1), U256::ZERO),
        Err(PriorityEconomicsValidationErrorV2::InvalidL1Tuple)
    );
    assert!(
        SelectedRouteEvidenceV2::new(
            B256::ZERO,
            digest(2),
            [address(1), address(2)],
            [address(3), address(4), address(5)],
            [digest(3), digest(4)],
            [500, 3_000],
            [true, false],
            U256::from(1_000),
            digest(5),
            digest(6),
            digest(7),
            digest(8),
            digest(9),
            digest(10),
        )
        .is_err()
    );
}

#[test]
fn v1_projection_preserves_nulls_counters_and_signed_values() {
    let unavailable = PriorityEconomicsV2::authority_unavailable(
        AdmissionStageV2::CandidateDiscoveryAttempted,
        AuthorityUnavailableV2::Discovery(
            DiscoveryAuthorityUnavailableV2::new(
                DiscoveryUnavailableReasonV2::Deadline,
                digest(15),
            )
            .unwrap(),
        ),
        None,
        None,
        counters([1, 0, 0, 0, 0, 0, 0, 0]),
    )
    .unwrap()
    .project_v1();
    assert_eq!(unavailable.disposition(), EconomicDispositionV1::AuthorityUnavailable);
    assert_eq!(unavailable.ev_wei_signed(), None);
    assert_eq!(unavailable.authority_attempted(), 0);
}

#[test]
fn optimism_missing_and_false_positive_mutants_are_rejected() {
    assert_eq!(evaluated(10, None), Err(PriorityEconomicsValidationErrorV2::OptimismFlagRequired));
    assert_eq!(
        evaluated(10, Some(false)),
        Err(PriorityEconomicsValidationErrorV2::OptimismFlagRequired)
    );

    let ledger = PriorityEconomicsLedgerV2::new(NonZeroUsize::new(1).unwrap());
    let invalid = evaluated(10, None).map_err(PriorityEconomicsLedgerErrorV2::from);
    assert_eq!(
        invalid,
        Err(PriorityEconomicsLedgerErrorV2::Validation(
            PriorityEconomicsValidationErrorV2::OptimismFlagRequired,
        ))
    );
    assert!(ledger.snapshot().unwrap().is_empty());
}
