#![doc = include_str!("../README.md")]

mod aggregate;
pub use aggregate::{Aggregate, ExpectedManifest, ExpectedScenario};

mod check;
pub use check::{ObservationState, ObservedBlock, RpcObserver};

mod cli;
pub use cli::{
    AcceptanceCli, AcceptanceCommand, CliRun, ExitCode, MatrixScenario, ScenarioMatrix,
    SelectionSuite,
};

mod config;
pub use config::{
    AcceptanceCheck, CheckStart, CiConfig, CiSuite, DevnetConfig, DevnetProfile, ForkActivation,
    ForwardingConfig, GlamsterdamFork, L1Config, L1Forks, L2Config, ReadinessConfig,
    ScenarioConfig, Span,
};

mod contract;
pub use contract::{
    B20CreateConfig, B20PrecompileClient, ContractCase, RegistryWorkload, TokenWorkload,
};

mod parity;
pub use parity::{FuzzTransactionGenerator, FuzzedTransaction, Parity, ParityWorkload};

mod provision;
pub use provision::{EndpointMap, Ownership, Provisioner};

mod protocol;
pub use protocol::{
    AuthenticatedHeader, BatchAttribution, BatchObserver, BlobEvidence, GlamsterdamCheck, Rpc,
    Schedule, Submission, Submissions, SubmittedChannel, Transfer, TransferRequest, TransferTarget,
};

mod publish;
pub use publish::{CommentMetadata, PrPublisher, PublishArgs};

mod report;
pub use report::{Report, ReportCounts};

mod result;
pub use result::{
    CheckResult, ForkBoundary, HeadSample, RunResult, ScenarioResult, StageResult, Status,
};

mod runner;
pub use runner::{AcceptanceOptions, AcceptanceRunner};

mod runtime;
pub use runtime::RuntimeCase;

mod transaction;
pub use transaction::{TransactionCase, TransactionWorkload};

mod workload;
pub use workload::WorkloadContext;
