#![doc = include_str!("../README.md")]

mod aggregate;
pub use aggregate::{Aggregate, ExpectedManifest, ExpectedScenario};
mod check;
pub use check::{ObservationState, ObservedBlock, RpcObserver};
mod cli;
pub use cli::{AcceptanceCli, AcceptanceCommand, CliRun, ExitCode};
mod config;
pub use config::{
    AcceptanceCheck, CheckStart, DevnetConfig, ForkActivation, L1Config, L2Config, ReadinessConfig,
    ScenarioConfig, Span,
};
mod provision;
pub use provision::{EndpointMap, Ownership, Provisioner};
mod result;
pub use result::{
    CheckResult, ForkBoundary, HeadSample, RunResult, ScenarioResult, StageResult, Status,
};
mod report;
pub use report::{Report, ReportCounts};
mod runner;
pub use runner::{AcceptanceOptions, AcceptanceRunner};
