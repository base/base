#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod envelope;
pub use envelope::{
    CandidateEnvelope, CandidateSelectionRule, CandidateSet as Eip712CandidateSet, FallbackMode,
    MAX_SIBLINGS_CEILING, RULE_MAX_RECIPIENT_BALANCE_DELTA, SCORE_VERSION_V1,
    SelectionRule as Eip712SelectionRule,
};

mod error;
pub use error::CandidateError;

mod group;
pub use group::{CandidateGroup, CandidateSibling};

mod validation;
pub use validation::{
    CandidateLimits, CandidateValidator, DEFAULT_MAX_REQUEST_BYTES, DEFAULT_MAX_SIBLING_BYTES,
    DEFAULT_MAX_SIBLING_GAS, DEFAULT_MAX_SIBLINGS, ExpiryWindow,
};

mod policy;
pub use policy::{
    AccessDenied, AttemptId, CandidateAccessPolicy, ChargeDecision, GroupOutcome, GroupSummary,
    PrincipalEntry, PrincipalId, PrincipalsFile, StaticAccessPolicy, WorkTicket,
};

mod ledger;
pub use ledger::{
    DEFAULT_MAX_EVALUATIONS, DEFAULT_MAX_GROUPS, DEFAULT_MAX_GROUPS_PER_SENDER,
    DEFAULT_RECOVERY_WINDOW_BLOCKS, EvictionReason, GroupLedger, GroupLedgerEntry,
    GroupReservation, GroupState, LedgerAdmission, LedgerLimits, ReserveError,
};

mod lifecycle;
pub use lifecycle::{
    DeferReason, FallbackReason, GroupLifecycle, ParkReason, TurnContext, TurnDecision,
};

mod selection;
pub use selection::{
    DEFAULT_PROBE_GAS, Ineligibility, ProbeResult, ScoreRead, TrialOutcome, WinnerSelection,
};

mod runtime;
pub use runtime::{CandidateRuntime, CandidateSwitch};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
